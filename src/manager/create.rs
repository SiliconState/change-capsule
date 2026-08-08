//! Capsule creation, including state-root-scoped idempotent creation.
//!
//! Part of the [`CapsuleManager`] implementation; see the parent module.

// This file is a continuation of the parent module's `impl CapsuleManager`,
// so it deliberately shares the parent's imports rather than duplicating a
// large, constantly churning list.
#[allow(clippy::wildcard_imports)]
use super::*;

impl CapsuleManager {
    /// Create a capsule and its isolated worktree from a pinned commit.
    pub fn create(&self, options: CreateOptions) -> Result<Capsule> {
        validate_create_options(&options)?;
        let repository = self.git.repository(&options.repository)?;
        let base_commit = self
            .git
            .resolve_commit(&repository.worktree, &options.base)?;
        let project_key = project_key(&repository.common_dir)?;
        let _global_lock = self.store.lock_global()?;
        let _lock = self.store.lock_project(&project_key)?;
        let id = new_capsule_id();
        let capsule =
            self.initial_capsule(id, options, &repository, project_key, base_commit, now()?)?;
        self.materialize_new_capsule(capsule, false)
    }

    /// Create or replay one state-root-scoped logical capsule creation.
    ///
    /// The key is opaque local orchestration state, not a credential. A durable
    /// reservation is published before any capsule identity or worktree side
    /// effect, and remains bound to the same capsule for its lifetime.
    pub fn create_idempotent(&self, options: CreateOptions, key: &str) -> Result<Capsule> {
        validate_create_options(&options)?;
        let key_digest = key_sha256(key)?;
        let repository = self.git.repository(&options.repository)?;
        let requested_project_key = project_key(&repository.common_dir)?;
        let _global_lock = self.store.lock_global()?;
        let _project_lock = self.store.lock_project(&requested_project_key)?;

        if let Some(record) = self.store.read_idempotency_record(&key_digest)? {
            self.validate_replayed_request(&options, &repository, &record)?;
            return self.materialize_idempotency_record(&record);
        }

        let base_commit = self
            .git
            .resolve_commit(&repository.worktree, &options.base)?;
        let record = IdempotencyRecord {
            schema_version: IDEMPOTENCY_RECORD_SCHEMA_VERSION,
            idempotency_key_sha256: key_digest,
            request_sha256: String::new(),
            record_sha256: String::new(),
            capsule_id: new_capsule_id(),
            source_worktree: repository.worktree.clone(),
            repository_common_dir: repository.common_dir.clone(),
            project_key: requested_project_key,
            base_selector: options.base,
            base_commit,
            label: options.label,
            links: options.links,
            reserved_at_unix: now()?,
        }
        .sealed()?;
        // Never publish a reservation this build could not read back. The reader
        // is deliberately strict, and a record that fails it would wedge the key
        // forever, so prove the round trip before taking the first side effect.
        record.validate(&record.idempotency_key_sha256)?;
        self.store.write_idempotency_record_new(&record)?;
        #[cfg(test)]
        run_idempotent_create_test_hook(IdempotentCreateTestStage::AfterReservation)?;
        self.materialize_idempotency_record(&record)
    }

    /// Directly resolve one state-root-scoped idempotency key without scans.
    pub fn lookup_idempotency_key(&self, key: &str) -> Result<IdempotencyLookup> {
        Self::lookup_idempotency_key_in_store(&self.store, key)
    }

    /// Open an existing state root and directly resolve one idempotency key.
    ///
    /// This lookup does not create state, acquire locks, enumerate capsules, or
    /// discover/invoke Git.
    pub fn lookup_idempotency_key_at(
        state_root: impl AsRef<Path>,
        key: &str,
    ) -> Result<IdempotencyLookup> {
        let store = StateStore::open_existing(state_root.as_ref())?;
        Self::lookup_idempotency_key_in_store(&store, key)
    }

    pub(super) fn lookup_idempotency_key_in_store(
        store: &StateStore,
        key: &str,
    ) -> Result<IdempotencyLookup> {
        let key_digest = key_sha256(key)?;
        let record = store
            .read_idempotency_record(&key_digest)?
            .ok_or(Error::IdempotencyNotFound)?;
        let capsule = if store.capsule_manifest_exists(&record.capsule_id)? {
            let capsule = store.read_capsule(&record.capsule_id)?;
            validate_reservation_capsule(&record, &capsule)?;
            Some(capsule)
        } else {
            store.validate_unmaterialized_capsule(&record.capsule_id)?;
            None
        };
        Ok(IdempotencyLookup {
            schema_version: IDEMPOTENCY_RECORD_SCHEMA_VERSION,
            idempotency_key_sha256: key_digest,
            capsule_id: record.capsule_id,
            status: if capsule.is_some() {
                IdempotencyStatus::Materialized
            } else {
                IdempotencyStatus::Reserved
            },
            capsule,
        })
    }

    pub(super) fn validate_replayed_request(
        &self,
        options: &CreateOptions,
        repository: &Repository,
        record: &IdempotencyRecord,
    ) -> Result<()> {
        if repository.worktree != record.source_worktree
            || repository.common_dir != record.repository_common_dir
            || project_key(&repository.common_dir)? != record.project_key
        {
            return Err(Error::IdempotencyConflict);
        }
        let base_commit = if options.base == record.base_selector {
            record.base_commit.clone()
        } else {
            match self.git.resolve_commit(&repository.worktree, &options.base) {
                Ok(commit) => commit,
                Err(_) => return Err(Error::IdempotencyConflict),
            }
        };
        if base_commit != record.base_commit {
            return Err(Error::IdempotencyConflict);
        }
        let equivalent = canonical_request_sha256(
            &repository.worktree,
            &repository.common_dir,
            &record.project_key,
            &record.base_selector,
            &base_commit,
            options.label.as_deref(),
            &options.links,
        )?;
        if equivalent != record.request_sha256 {
            return Err(Error::IdempotencyConflict);
        }
        Ok(())
    }

    pub(super) fn materialize_idempotency_record(
        &self,
        record: &IdempotencyRecord,
    ) -> Result<Capsule> {
        if self.store.capsule_manifest_exists(&record.capsule_id)? {
            let mut capsule = self.store.read_capsule(&record.capsule_id)?;
            validate_reservation_capsule(record, &capsule)?;
            if capsule.state == CapsuleState::Creating {
                self.complete_creating(&mut capsule, true)?;
            }
            return Ok(capsule);
        }
        self.store
            .validate_unmaterialized_capsule(&record.capsule_id)?;
        let mut options = CreateOptions::new(record.source_worktree.clone())
            .with_base(record.base_selector.clone())
            .with_links(record.links.clone());
        options.label.clone_from(&record.label);
        // Resolve the source repository as it exists now and require it to still
        // be the one the reservation was made against. Adopting the reserved
        // identity against a replaced repository would bind a capsule to a base
        // commit that never came from it.
        let repository = self.git.repository(&record.source_worktree)?;
        if repository.worktree != record.source_worktree
            || repository.common_dir != record.repository_common_dir
        {
            return Err(Error::UnsafeState(
                "reserved source repository identity changed before creation".to_owned(),
            ));
        }
        let capsule = self.initial_capsule(
            record.capsule_id.clone(),
            options,
            &repository,
            record.project_key.clone(),
            record.base_commit.clone(),
            record.reserved_at_unix,
        )?;
        self.materialize_new_capsule(capsule, true)
    }

    pub(super) fn initial_capsule(
        &self,
        id: String,
        options: CreateOptions,
        repository: &Repository,
        project_key: String,
        base_commit: String,
        created_at: u64,
    ) -> Result<Capsule> {
        let branch = format!("capsule/{}", &id[4..]);
        let workspace_path = self.store.workspace_path(&project_key, &id)?;
        Ok(Capsule {
            schema_version: SCHEMA_VERSION,
            id,
            label: options.label,
            links: options.links,
            state: CapsuleState::Creating,
            source_worktree: repository.worktree.clone(),
            repository_common_dir: repository.common_dir.clone(),
            workspace_git_dir: None,
            workspace_path,
            project_key,
            branch,
            base_commit,
            created_at_unix: created_at,
            updated_at_unix: created_at,
            checkpoints: Vec::new(),
            checkpoint: None,
            evidence: Vec::new(),
            result: None,
            integration: None,
            cleanup: None,
            closed_at_unix: None,
            dropped_at_unix: None,
        })
    }

    /// Publish the manifest and finish the Git side of a `creating` capsule.
    ///
    /// `reserved` marks an identity bound to an idempotency reservation. Such an
    /// identity can never be replaced, so an unprovable Git state orphans that
    /// same capsule. A plain create owns a brand-new identity nothing refers to
    /// yet, so it keeps its original contract and fails instead.
    pub(super) fn materialize_new_capsule(
        &self,
        mut capsule: Capsule,
        reserved: bool,
    ) -> Result<Capsule> {
        if reserved {
            self.store
                .prepare_reserved_capsule(&capsule.id, &capsule.project_key)?;
        } else {
            self.store
                .prepare_capsule(&capsule.id, &capsule.project_key)?;
        }
        self.store.write_capsule(&capsule)?;
        #[cfg(test)]
        if reserved {
            run_idempotent_create_test_hook(IdempotentCreateTestStage::AfterManifest)?;
        }
        self.complete_creating(&mut capsule, reserved)?;
        Ok(capsule)
    }

    /// Drive a `creating` capsule to `active`, or refuse to guess.
    ///
    /// With `allow_orphan`, an unprovable Git state marks this same capsule
    /// orphaned and succeeds, because the caller's identity is already durably
    /// bound and must never be silently replaced. Without it, the same condition
    /// is an error.
    pub(super) fn complete_creating(
        &self,
        capsule: &mut Capsule,
        allow_orphan: bool,
    ) -> Result<String> {
        let Ok(source) = self.git.repository(&capsule.source_worktree) else {
            return self.abandon_creation(
                capsule,
                "reserved source repository is no longer available",
                allow_orphan,
            );
        };
        if source.common_dir != capsule.repository_common_dir {
            return self.abandon_creation(
                capsule,
                "reserved source repository identity no longer agrees",
                allow_orphan,
            );
        }
        if path_entry_exists_no_follow(&capsule.workspace_path)? {
            if capsule.workspace_git_dir.is_none() {
                capsule.workspace_git_dir = self
                    .git
                    .repository(&capsule.workspace_path)
                    .ok()
                    .map(|repository| repository.git_dir);
            }
            if self.validate_owned_worktree(capsule).is_ok() {
                let head = self.git.head(&capsule.workspace_path)?;
                let branch_head = self.git.branch_head(&source.worktree, &capsule.branch)?;
                if head != capsule.base_commit
                    || branch_head.as_deref() != Some(capsule.base_commit.as_str())
                    || (!self.git.clean(&capsule.workspace_path)?
                        && !is_unchecked_worktree_shape(&capsule.workspace_path)?)
                {
                    return self.abandon_creation(
                        capsule,
                        "existing worktree does not exactly match the reserved base",
                        allow_orphan,
                    );
                }
                self.git
                    .finish_worktree_creation(&capsule.workspace_path, &capsule.base_commit)?;
                return self.activate_created_capsule(capsule);
            }
            return self.abandon_creation(capsule, "workspace path is contradictory", allow_orphan);
        }
        let branch_head = self.git.branch_head(&source.worktree, &capsule.branch)?;
        let records = self.git.registered_worktrees(&source.worktree)?;
        if branch_head.is_some()
            || records.iter().any(|record| {
                record.branch.as_deref() == Some(capsule.branch.as_str())
                    || same_path_existing_or_clean(&record.path, &capsule.workspace_path)
            })
        {
            return self.abandon_creation(
                capsule,
                "partial Git creation state is contradictory",
                allow_orphan,
            );
        }
        self.git.add_worktree(
            &source.worktree,
            &capsule.workspace_path,
            &capsule.branch,
            &capsule.base_commit,
        )?;
        capsule.workspace_git_dir = Some(self.git.repository(&capsule.workspace_path)?.git_dir);
        self.validate_owned_worktree(capsule)?;
        self.activate_created_capsule(capsule)
    }

    pub(super) fn activate_created_capsule(&self, capsule: &mut Capsule) -> Result<String> {
        capsule.state = CapsuleState::Active;
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(capsule)?;
        Ok("completed an interrupted workspace creation".to_owned())
    }

    pub(super) fn abandon_creation(
        &self,
        capsule: &mut Capsule,
        reason: &str,
        allow_orphan: bool,
    ) -> Result<String> {
        if !allow_orphan {
            return Err(Error::UnsafeState(format!(
                "cannot create capsule {}: {reason}",
                capsule.id
            )));
        }
        capsule.state = CapsuleState::Orphaned;
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(capsule)?;
        Ok(format!(
            "marked an incomplete creation orphaned: {reason}; explicit inspection is required"
        ))
    }
}
