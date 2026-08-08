use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::artifact::{ArtifactReader, ArtifactSink, PublishedArtifact};
use crate::capabilities::{
    IDEMPOTENCY_RECORD_SCHEMA_VERSION, LABEL_BYTES_LIMIT, LINK_KEY_BYTES_LIMIT,
    LINK_VALUE_BYTES_LIMIT, LINKS_LIMIT,
};
use crate::error::{Error, Result, io};
use crate::git::{CommitPatch, Git, Repository};
use crate::idempotency::{
    IdempotencyLookup, IdempotencyRecord, IdempotencyStatus, canonical_request_sha256, key_sha256,
};
use crate::model::{
    AUDIT_EVENT_CAP, AUDIT_SCHEMA_VERSION, ArtifactBundle, ArtifactDescriptor, ArtifactKind,
    AuditEvent, AuditEventKind, BUNDLE_SCHEMA_VERSION, BackupReport, Capsule, CapsuleHealth,
    CapsuleResult, CapsuleState, CapsuleStatus, CapsuleSummary, Checkpoint, CheckpointJournal,
    Cleanup, Evidence, ExportReport, Integration, MetricsSnapshot, RecoveryAction, ResultKind,
    ResultRef, SCHEMA_VERSION, StateInspection,
};
use crate::policy::{Policy, PolicyReport};
use crate::state::{StateStore, default_state_root, project_key};

const LABEL_CAP: usize = LABEL_BYTES_LIMIT;
const LINK_KEY_CAP: usize = LINK_KEY_BYTES_LIMIT;
const LINK_VALUE_CAP: usize = LINK_VALUE_BYTES_LIMIT;
const MESSAGE_CAP: usize = 16 * 1024;
const EVIDENCE_COMMAND_CAP: usize = 16 * 1024;
const EVIDENCE_SUMMARY_CAP: usize = 64 * 1024;
const EVIDENCE_COUNT_CAP: usize = 64;
const EVIDENCE_TOTAL_BYTES_CAP: usize = 256 * 1024;
const CHECKPOINT_COUNT_CAP: usize = 128;
const DEFAULT_AUTHOR_NAME: &str = "Capsule";
const DEFAULT_AUTHOR_EMAIL: &str = "capsule@localhost";

/// Commit identity used for checkpoints and integration commits.
///
/// Always explicit: this crate never reads the ambient Git configuration to
/// decide who authored a change it creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    /// Name recorded as both author and committer.
    pub name: String,
    /// Email recorded as both author and committer. Must contain `@`.
    pub email: String,
}

impl Default for Author {
    fn default() -> Self {
        Self {
            name: DEFAULT_AUTHOR_NAME.to_owned(),
            email: DEFAULT_AUTHOR_EMAIL.to_owned(),
        }
    }
}

/// Inputs for creating a capsule.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Any path inside the source Git repository.
    pub repository: PathBuf,
    /// Revision to pin, resolved to an immutable commit before work starts.
    pub base: String,
    /// Optional human-facing description of the attempt.
    pub label: Option<String>,
    /// Opaque caller metadata, at most 32 entries. No key is privileged.
    pub links: BTreeMap<String, String>,
}

impl CreateOptions {
    /// Options for a capsule based on the repository's current `HEAD`.
    pub fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
            base: "HEAD".to_owned(),
            label: None,
            links: BTreeMap::new(),
        }
    }
}

/// Inputs for committing the workspace's current state as a checkpoint.
#[derive(Debug, Clone)]
pub struct CheckpointOptions {
    /// Message for the checkpoint commit.
    pub message: String,
    /// Identity to record on the commit.
    pub author: Author,
}

/// A verification claim to attach to a capsule.
#[derive(Debug, Clone)]
pub struct EvidenceInput {
    /// Exact command the caller ran.
    pub command: String,
    /// Exit status the caller observed.
    pub exit_code: i32,
    /// Optional bounded summary of what happened.
    pub summary: Option<String>,
}

/// Requirements a capsule must satisfy before its result is sealed.
#[derive(Debug, Clone, Copy, Default)]
pub struct CloseOptions {
    /// Refuse to seal unless evidence exists and every exit code is zero.
    pub require_successful_evidence: bool,
    /// Refuse to seal unless successful evidence is bound to the patch being sealed.
    pub require_current_successful_evidence: bool,
}

/// Inputs for applying a sealed result to a target worktree.
#[derive(Debug, Clone)]
pub struct IntegrateOptions {
    /// Any path inside the destination worktree.
    ///
    /// It must belong to the same repository, be clean, and still be at the
    /// capsule's pinned base.
    pub target: PathBuf,
    /// Commit subject. Defaults to the capsule label, then a generated subject.
    pub message: Option<String>,
    /// Identity to record on the integration commit.
    pub author: Author,
}

/// Entry point for every capsule operation.
///
/// A manager owns one state directory and resolves the Git executable once when
/// opened. Instances are cheap; operations serialize across processes through
/// file locks, so several managers may safely share a state root.
///
/// # Example
///
/// ```no_run
/// use change_capsule::{CapsuleManager, CreateOptions};
///
/// let manager = CapsuleManager::open_default()?;
/// let capsule = manager.create(CreateOptions::new("."))?;
/// println!("work in {}", capsule.workspace_path.display());
/// # Ok::<(), change_capsule::Error>(())
/// ```
#[derive(Debug)]
pub struct CapsuleManager {
    store: StateStore,
    git: Git,
}

impl CapsuleManager {
    /// Open a manager on the default state directory.
    ///
    /// Honours `CAPSULE_HOME`, then the platform state directory.
    pub fn open_default() -> Result<Self> {
        Self::open(default_state_root()?)
    }

    /// Open a manager on an explicit state directory, creating it if needed.
    pub fn open(state_root: impl Into<PathBuf>) -> Result<Self> {
        let state_root = state_root.into();
        Ok(Self {
            store: StateStore::open(&state_root)?,
            git: Git::discover()?,
        })
    }

    /// Directory holding this manager's durable state.
    pub fn state_root(&self) -> &Path {
        self.store.root()
    }

    /// Read the effective policy; permissive defaults when none is installed.
    pub fn policy(&self) -> Result<Policy> {
        self.store.read_policy()
    }

    /// Replace the stored policy atomically.
    ///
    /// Repository roots are canonicalized and must be existing directories.
    pub fn set_policy(&self, mut policy: Policy) -> Result<Policy> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        for root in &mut policy.allowed_repository_roots {
            *root = canonical_existing(root)?;
            if !root.is_dir() {
                return Err(Error::PolicyViolation(format!(
                    "allowed repository root is not a directory: {}",
                    root.display()
                )));
            }
        }
        policy.allowed_repository_roots.sort();
        policy.allowed_repository_roots.dedup();
        policy.validate()?;
        self.store.write_policy(&policy)?;
        Ok(policy)
    }

    /// Evaluate current state against the effective policy without mutating it.
    pub fn policy_report(&self) -> Result<PolicyReport> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        let policy = self.store.read_policy()?;
        let capsules = self.store.list_capsules()?;
        let violations = self.policy_violations(&policy, &capsules)?;
        Ok(PolicyReport {
            compliant: violations.is_empty(),
            violations,
        })
    }

    /// Summarize stored records, including ones whose schema is unsupported.
    pub fn inspect_state(&self) -> Result<StateInspection> {
        self.store.inspect()
    }

    /// Copy durable manifests, results, patches, and policy to a new directory.
    ///
    /// Live workspaces and Git object databases are deliberately excluded.
    /// `backup.json` is written last as the completion marker.
    pub fn backup_state(&self, destination: impl AsRef<Path>) -> Result<BackupReport> {
        self.store.backup(destination.as_ref())
    }

    /// Explicitly migrate schema-v3 durable manifests/results to schema v4.
    ///
    /// Dry-run validates every candidate without mutation. Apply requires a new
    /// external backup destination and writes the complete backup before state.
    pub fn migrate_state_v3(
        &self,
        backup: Option<&Path>,
        apply: bool,
    ) -> Result<crate::model::MigrationReport> {
        self.store.migrate_v3(backup, apply)
    }

    /// Retained lifecycle events for one capsule, oldest first.
    pub fn audit_events(&self, id: &str) -> Result<Vec<AuditEvent>> {
        Ok(self.show(id)?.audit_events)
    }

    /// Retained lifecycle events across all capsules, merged in time order.
    pub fn audit_log(&self) -> Result<Vec<AuditEvent>> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        let mut sequenced = Vec::new();
        for capsule in self.store.list_capsules()? {
            for (sequence, event) in capsule.audit_events.into_iter().enumerate() {
                sequenced.push((capsule.id.clone(), sequence, event));
            }
        }
        sequenced.sort_by(|left, right| {
            left.2
                .occurred_at_unix
                .cmp(&right.2.occurred_at_unix)
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| left.1.cmp(&right.1))
        });
        Ok(sequenced.into_iter().map(|(_, _, event)| event).collect())
    }

    /// Compute an instantaneous snapshot of aggregate counters.
    pub fn metrics(&self) -> Result<MetricsSnapshot> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        let capsules = self.store.list_capsules()?;
        let mut states = BTreeMap::new();
        let mut live_capsules = 0_u64;
        let mut sealed_results = 0_u64;
        let mut result_patch_bytes = 0_u64;
        let mut audit_events = 0_u64;
        let mut audit_events_dropped = 0_u64;
        for capsule in &capsules {
            *states
                .entry(state_name(capsule.state).to_owned())
                .or_insert(0) += 1;
            if capsule.state != CapsuleState::Dropped {
                live_capsules += 1;
            }
            if let Some(result) = &capsule.result {
                sealed_results += 1;
                result_patch_bytes = result_patch_bytes.saturating_add(result.patch_bytes);
            }
            audit_events = audit_events.saturating_add(capsule.audit_events.len() as u64);
            audit_events_dropped =
                audit_events_dropped.saturating_add(capsule.audit_events_dropped);
        }
        Ok(MetricsSnapshot {
            observed_at_unix: now()?,
            capsules: capsules.len() as u64,
            live_capsules,
            sealed_results,
            result_patch_bytes,
            state_bytes: self.store.state_bytes()?,
            workspace_bytes: self.store.workspace_bytes()?,
            audit_events,
            audit_events_dropped,
            states,
        })
    }

    /// Describe the sealed artifacts of a closed capsule.
    ///
    /// Revalidates the seal first, so descriptors always match live bytes.
    pub fn artifacts(&self, id: &str) -> Result<ArtifactBundle> {
        self.artifact_snapshot(id).map(|(bundle, _, _)| bundle)
    }

    /// Open one sealed artifact as a bounded reader over a validated snapshot.
    pub fn open_artifact(&self, id: &str, kind: ArtifactKind) -> Result<ArtifactReader> {
        let (_, result, patch) = self.artifact_snapshot(id)?;
        Ok(ArtifactReader::new(match kind {
            ArtifactKind::ResultManifest => result,
            ArtifactKind::ResultPatch => patch,
        }))
    }

    /// Stream every sealed artifact into a caller-provided sink.
    pub fn publish_artifacts<S: ArtifactSink + ?Sized>(
        &self,
        id: &str,
        sink: &mut S,
    ) -> Result<Vec<PublishedArtifact>> {
        let (bundle, result, patch) = self.artifact_snapshot(id)?;
        let mut result = Some(result);
        let mut patch = Some(patch);
        let mut published = Vec::with_capacity(bundle.artifacts.len());
        for descriptor in bundle.artifacts {
            let bytes = match descriptor.kind {
                ArtifactKind::ResultManifest => result.take(),
                ArtifactKind::ResultPatch => patch.take(),
            }
            .ok_or_else(|| {
                Error::UnsafeState(format!(
                    "artifact bundle contains duplicate kind {:?}",
                    descriptor.kind
                ))
            })?;
            let mut source = ArtifactReader::new(bytes);
            let uri = sink.put(&descriptor, &mut source)?;
            published.push(PublishedArtifact { descriptor, uri });
        }
        Ok(published)
    }

    /// Write a sealed result to a new directory as a portable receipt.
    ///
    /// Produces `result.json`, `result.patch`, and finally `bundle.json` as the
    /// completion marker. Verify the directory later with
    /// [`verify_bundle`](crate::verify_bundle).
    pub fn export_artifacts(
        &self,
        id: &str,
        destination: impl AsRef<Path>,
    ) -> Result<ExportReport> {
        let (bundle, result, patch) = self.artifact_snapshot(id)?;
        let destination = self.store.external_destination(destination.as_ref())?;
        let mut exported_bundle = bundle.clone();
        for descriptor in &mut exported_bundle.artifacts {
            // Reference artifacts relative to the bundle rather than by absolute
            // path. A receipt is meant to travel: an exporting machine's
            // directory layout is meaningless to whoever verifies it later, and
            // embedding it would publish that layout in any repository or
            // artifact store the receipt is committed to.
            descriptor.uri = descriptor.name.clone();
        }
        let mut manifest =
            serde_json::to_vec_pretty(&exported_bundle).map_err(|source| Error::Json {
                path: PathBuf::from("bundle.json"),
                source,
            })?;
        manifest.push(b'\n');
        StateStore::export_artifacts(
            &destination,
            &[
                ("bundle.json", &manifest),
                ("result.json", &result),
                ("result.patch", &patch),
            ],
        )?;
        Ok(ExportReport {
            bundle: exported_bundle,
            output_directory: destination,
        })
    }

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
        let policy = self.store.read_policy()?;
        let capsules = self.store.list_capsules()?;
        let pending_reservations =
            if policy.max_capsules.is_some() || policy.max_live_capsules.is_some() {
                self.store.unmaterialized_idempotency_count()?
            } else {
                0
            };
        self.enforce_create_policy(&policy, &capsules, &repository, pending_reservations)?;
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
        let policy = self.store.read_policy()?;
        let (capsules, pending_reservations) =
            if policy.max_capsules.is_some() || policy.max_live_capsules.is_some() {
                (
                    self.store.list_capsules()?,
                    self.store.unmaterialized_idempotency_count()?,
                )
            } else {
                (Vec::new(), 0)
            };
        self.enforce_create_policy(&policy, &capsules, &repository, pending_reservations)?;
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

    fn lookup_idempotency_key_in_store(store: &StateStore, key: &str) -> Result<IdempotencyLookup> {
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

    fn validate_replayed_request(
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

    fn materialize_idempotency_record(&self, record: &IdempotencyRecord) -> Result<Capsule> {
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
        let options = CreateOptions {
            repository: record.source_worktree.clone(),
            base: record.base_selector.clone(),
            label: record.label.clone(),
            links: record.links.clone(),
        };
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

    fn initial_capsule(
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
            audit_events: Vec::new(),
            audit_events_dropped: 0,
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
    fn materialize_new_capsule(&self, mut capsule: Capsule, reserved: bool) -> Result<Capsule> {
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
    fn complete_creating(&self, capsule: &mut Capsule, allow_orphan: bool) -> Result<String> {
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

    fn activate_created_capsule(&self, capsule: &mut Capsule) -> Result<String> {
        capsule.state = CapsuleState::Active;
        capsule.updated_at_unix = now()?;
        let base_commit = capsule.base_commit.clone();
        append_event(
            capsule,
            AuditEventKind::Created,
            Some(CapsuleState::Creating),
            CapsuleState::Active,
            BTreeMap::from([("base_commit".to_owned(), base_commit)]),
        )?;
        self.store.write_capsule(capsule)?;
        Ok("completed an interrupted workspace creation".to_owned())
    }

    fn abandon_creation(
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
        let previous = capsule.state;
        capsule.state = CapsuleState::Orphaned;
        capsule.updated_at_unix = now()?;
        append_event(
            capsule,
            AuditEventKind::Recovered,
            Some(previous),
            CapsuleState::Orphaned,
            BTreeMap::from([("reason".to_owned(), reason.to_owned())]),
        )?;
        self.store.write_capsule(capsule)?;
        Ok(format!(
            "marked an incomplete creation orphaned: {reason}; explicit inspection is required"
        ))
    }

    /// Summarize every durable capsule record, ordered by identifier.
    pub fn list(&self) -> Result<Vec<CapsuleSummary>> {
        let _lock = self.store.lock_global()?;
        Ok(self
            .store
            .list_capsules()?
            .iter()
            .map(CapsuleSummary::from)
            .collect())
    }

    /// Read one capsule manifest, revalidating its identity fields.
    pub fn show(&self, id: &str) -> Result<Capsule> {
        self.store
            .read_capsule(id)
            .map_err(|error| not_found(id, error))
    }

    /// Filesystem directory a capsule's attempt works in.
    pub fn workspace_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self.show(id)?.workspace_path)
    }

    /// Inspect a capsule's workspace health, changes, and seal state.
    pub fn status(&self, id: &str) -> Result<CapsuleStatus> {
        let capsule = self.show(id)?;
        self.status_for(capsule)
    }

    /// Complete patch from the pinned base: sealed if closed, live if active.
    pub fn diff(&self, id: &str) -> Result<Vec<u8>> {
        let capsule = self.show(id)?;
        match capsule.state {
            CapsuleState::Closed | CapsuleState::Integrating | CapsuleState::Integrated => self
                .sealed_artifacts(&capsule)
                .and_then(|(matches, _, patch)| {
                    if matches {
                        Ok(patch)
                    } else {
                        Err(Error::ResultDrift(capsule.id.clone()))
                    }
                }),
            CapsuleState::Dropping | CapsuleState::Dropped if capsule.result.is_some() => self
                .sealed_artifacts(&capsule)
                .and_then(|(matches, _, patch)| {
                    if matches {
                        Ok(patch)
                    } else {
                        Err(Error::ResultDrift(capsule.id.clone()))
                    }
                }),
            CapsuleState::Active | CapsuleState::Checkpointing => {
                self.validate_owned_worktree(&capsule)?;
                let snapshot = self.snapshot(&capsule)?;
                Ok(snapshot.patch)
            }
            CapsuleState::Creating | CapsuleState::Orphaned => {
                Err(invalid_state(&capsule, "active or a sealed result"))
            }
            CapsuleState::Dropping | CapsuleState::Dropped => Ok(Vec::new()),
        }
    }

    /// Read a sealed result, failing on drift.
    pub fn result(&self, id: &str) -> Result<CapsuleResult> {
        let capsule = self.show(id)?;
        if capsule.result.is_none() {
            return Err(invalid_state(
                &capsule,
                "closed, integrating, integrated, or dropped result",
            ));
        }
        let (matches, result, _) = self.sealed_artifacts(&capsule)?;
        if !matches {
            return Err(Error::ResultDrift(capsule.id));
        }
        Ok(result)
    }

    /// Path to the sealed patch file, after confirming it still matches its seal.
    pub fn result_patch_path(&self, id: &str) -> Result<PathBuf> {
        let capsule = self.show(id)?;
        if capsule.result.is_none() {
            return Err(invalid_state(&capsule, "a sealed result"));
        }
        let (matches, _, _) = self.sealed_artifacts(&capsule)?;
        if !matches {
            return Err(Error::ResultDrift(capsule.id));
        }
        Ok(self.store.capsule_dir(id)?.join("result.patch"))
    }

    /// Commit the workspace's current state as a durable checkpoint.
    ///
    /// The commit is built through a private index and journaled before the
    /// capsule branch advances, so an interrupted checkpoint is recoverable.
    pub fn checkpoint(&self, id: &str, options: CheckpointOptions) -> Result<Checkpoint> {
        validate_message(&options.message, "checkpoint message")?;
        validate_author(&options.author)?;
        let mut capsule = self.show(id)?;
        let _global_lock = self.store.lock_global()?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Active, "active")?;
        let policy = self.enforce_capsule_policy(&capsule)?;
        self.validate_owned_worktree(&capsule)?;
        let head_before = self.git.head(&capsule.workspace_path)?;
        let checkpoint_snapshot = self.snapshot_against(&capsule, &head_before)?;
        let (ignored_paths, ignored_bytes) =
            self.ignored_usage_for_policy(&policy, &capsule.workspace_path)?;
        if checkpoint_snapshot.patch.is_empty() {
            return Err(Error::InvalidInput("nothing to checkpoint".to_owned()));
        }
        let index = self.store.temporary_index(&capsule.id)?;
        let head_after = self.git.commit_patch(&CommitPatch {
            worktree: &capsule.workspace_path,
            base: &head_before,
            patch: &checkpoint_snapshot.patch,
            index: index.path(),
            message: &options.message,
            name: &options.author.name,
            email: &options.author.email,
        })?;
        let prepared =
            self.git
                .commit_snapshot(&capsule.workspace_path, &head_before, &head_after)?;
        if self.git.parents(&capsule.workspace_path, &head_after)? != [head_before.clone()]
            || prepared.patch != checkpoint_snapshot.patch
            || prepared.changed_paths != checkpoint_snapshot.changed_paths
        {
            return Err(Error::UnsafeState(
                "prepared checkpoint commit does not reproduce the workspace snapshot".to_owned(),
            ));
        }
        let result_snapshot =
            self.git
                .commit_snapshot(&capsule.workspace_path, &capsule.base_commit, &head_after)?;
        Self::enforce_result_policy(
            &policy,
            result_snapshot.patch.len() as u64,
            result_snapshot.changed_paths.len(),
            ignored_paths,
            ignored_bytes,
        )?;
        // Prove the manifest can still record this checkpoint before the branch
        // advances. Failing afterwards would strand the capsule in
        // `Checkpointing`, and every recovery attempt would fail the same way.
        let started_at = now()?;
        let journal = CheckpointJournal {
            head_before: head_before.clone(),
            head_after: head_after.clone(),
            patch_sha256: sha256_hex(&checkpoint_snapshot.patch),
            message: options.message,
            author_name: options.author.name,
            author_email: options.author.email,
            started_at_unix: started_at,
        };
        Self::project_checkpoint(&capsule, &journal)?;
        self.git.create_ref(
            &capsule.workspace_path,
            &checkpoint_ref(&capsule),
            &head_after,
        )?;
        capsule.state = CapsuleState::Checkpointing;
        capsule.checkpoint = Some(journal);
        capsule.updated_at_unix = started_at;
        self.store.write_capsule(&capsule)?;

        let checkpoint = self.finish_checkpoint(&mut capsule)?.ok_or_else(|| {
            Error::UnsafeState("checkpoint side effect was not observable".to_owned())
        })?;
        capsule.updated_at_unix = now()?;
        append_event(
            &mut capsule,
            AuditEventKind::Checkpointed,
            Some(CapsuleState::Checkpointing),
            CapsuleState::Active,
            BTreeMap::from([("commit".to_owned(), checkpoint.commit.clone())]),
        )?;
        self.store.write_capsule(&capsule)?;
        Ok(checkpoint)
    }

    /// Attach a verification claim to an active capsule.
    ///
    /// Bounded to 64 records and 256 KiB of text per capsule.
    pub fn add_evidence(&self, id: &str, input: EvidenceInput) -> Result<Evidence> {
        validate_bounded_text(
            &input.command,
            EVIDENCE_COMMAND_CAP,
            "evidence command",
            false,
        )?;
        if let Some(summary) = &input.summary {
            validate_bounded_text(summary, EVIDENCE_SUMMARY_CAP, "evidence summary", true)?;
        }
        let mut capsule = self.show(id)?;
        let _global_lock = self.store.lock_global()?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Active, "active")?;
        self.enforce_capsule_policy(&capsule)?;
        self.validate_owned_worktree(&capsule)?;
        if capsule.evidence.len() >= EVIDENCE_COUNT_CAP {
            return Err(Error::InvalidInput(format!(
                "a capsule retains at most {EVIDENCE_COUNT_CAP} evidence records"
            )));
        }
        let stored_evidence_bytes: usize = capsule
            .evidence
            .iter()
            .map(|item| item.command.len() + item.summary.as_ref().map_or(0, String::len))
            .sum();
        let pending_bytes = input.command.len() + input.summary.as_ref().map_or(0, String::len);
        if stored_evidence_bytes.saturating_add(pending_bytes) > EVIDENCE_TOTAL_BYTES_CAP {
            return Err(Error::InvalidInput(format!(
                "total evidence payload would exceed the {EVIDENCE_TOTAL_BYTES_CAP}-byte capsule bound"
            )));
        }
        let snapshot = self.snapshot(&capsule)?;
        let evidence = Evidence {
            command: input.command,
            exit_code: input.exit_code,
            summary: input.summary,
            patch_sha256: Some(sha256_hex(&snapshot.patch)),
            recorded_at_unix: now()?,
        };
        // The byte caps above count raw input; JSON escaping can still inflate
        // it, so confirm the encoded manifest fits before recording anything.
        let mut projected = capsule.clone();
        projected.evidence.push(evidence.clone());
        crate::state::ensure_manifest_capacity(&projected)?;
        capsule.evidence.push(evidence.clone());
        capsule.updated_at_unix = now()?;
        let evidence_index = capsule.evidence.len() - 1;
        append_event(
            &mut capsule,
            AuditEventKind::EvidenceAdded,
            Some(CapsuleState::Active),
            CapsuleState::Active,
            BTreeMap::from([
                ("evidence_index".to_owned(), evidence_index.to_string()),
                (
                    "command_sha256".to_owned(),
                    sha256_hex(evidence.command.as_bytes()),
                ),
                ("exit_code".to_owned(), evidence.exit_code.to_string()),
            ]),
        )?;
        self.store.write_capsule(&capsule)?;
        Ok(evidence)
    }

    /// Seal the capsule into an immutable result and patch.
    ///
    /// Captures committed, staged, unstaged, deleted, and non-ignored untracked
    /// content as one complete change against the pinned base.
    pub fn close(&self, id: &str, options: CloseOptions) -> Result<CapsuleResult> {
        let mut capsule = self.show(id)?;
        let _global_lock = self.store.lock_global()?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Active, "active")?;
        let policy = self.enforce_capsule_policy(&capsule)?;
        self.validate_owned_worktree(&capsule)?;
        if options.require_successful_evidence
            && (capsule.evidence.is_empty()
                || capsule.evidence.iter().any(|item| item.exit_code != 0))
        {
            return Err(Error::InvalidInput(
                "successful evidence is required, but evidence is absent or contains failures"
                    .to_owned(),
            ));
        }
        let CloseSnapshotTransaction {
            clean,
            snapshot,
            head,
            ignored:
                IgnoredContentInventory {
                    paths: ignored_paths,
                    bytes: ignored_bytes,
                    content_sha256: ignored_content_sha256,
                },
        } = self.close_snapshot_transaction(&capsule)?;
        let digest = sha256_hex(&snapshot.patch);
        if options.require_current_successful_evidence
            && !capsule.evidence.iter().any(|item| {
                item.exit_code == 0 && item.patch_sha256.as_deref() == Some(digest.as_str())
            })
        {
            return Err(Error::InvalidInput(
                "current successful evidence is required, but no successful caller claim is bound to the exact final patch being sealed"
                    .to_owned(),
            ));
        }
        Self::enforce_result_policy(
            &policy,
            snapshot.patch.len() as u64,
            snapshot.changed_paths.len(),
            ignored_paths.len(),
            ignored_bytes,
        )?;
        let kind = if snapshot.patch.is_empty() {
            ResultKind::NoChange
        } else if clean {
            ResultKind::Commit
        } else {
            ResultKind::Patch
        };
        let sealed_at = now()?;
        let result = CapsuleResult {
            schema_version: SCHEMA_VERSION,
            capsule_id: capsule.id.clone(),
            label: capsule.label.clone(),
            links: capsule.links.clone(),
            kind,
            base_commit: capsule.base_commit.clone(),
            head_commit: head.clone(),
            patch_sha256: digest.clone(),
            patch_bytes: snapshot.patch.len() as u64,
            changed_paths: snapshot.changed_paths.clone(),
            ignored_bytes,
            ignored_content_sha256: Some(ignored_content_sha256),
            ignored_paths,
            checkpoints: capsule.checkpoints.clone(),
            evidence: capsule.evidence.clone(),
            created_at_unix: capsule.created_at_unix,
            sealed_at_unix: sealed_at,
        };
        let result_digest = result_sha256(&result)?;
        self.store.write_patch(id, &snapshot.patch)?;
        self.store.write_result(id, &result)?;
        capsule.result = Some(ResultRef {
            kind,
            head_commit: head,
            patch_sha256: digest,
            result_sha256: result_digest,
            patch_bytes: snapshot.patch.len() as u64,
            changed_paths: snapshot.changed_paths.len(),
            sealed_at_unix: sealed_at,
        });
        capsule.state = CapsuleState::Closed;
        capsule.closed_at_unix = Some(sealed_at);
        capsule.updated_at_unix = sealed_at;
        append_event(
            &mut capsule,
            AuditEventKind::Closed,
            Some(CapsuleState::Active),
            CapsuleState::Closed,
            BTreeMap::from([
                ("patch_sha256".to_owned(), result.patch_sha256.clone()),
                ("patch_bytes".to_owned(), result.patch_bytes.to_string()),
            ]),
        )?;
        self.store.write_capsule(&capsule)?;
        Ok(result)
    }

    /// Apply a sealed result to a clean target still at the pinned base.
    ///
    /// The candidate commit is built in a private index, checked for one exact
    /// parent and byte-identical reproduction of the sealed patch, protected by
    /// a namespaced ref, and only then fast-forwarded. Never rebases, merges,
    /// resolves conflicts, or pushes.
    pub fn integrate(&self, id: &str, options: &IntegrateOptions) -> Result<Capsule> {
        validate_author(&options.author)?;
        if let Some(message) = &options.message {
            validate_message(message, "integration message")?;
        }
        let mut capsule = self.show(id)?;
        let _global_lock = self.store.lock_global()?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Closed, "closed")?;
        let policy = self.enforce_capsule_policy(&capsule)?;
        self.ensure_sealed(&capsule)?;
        let (target, target_before, target_head_ref) =
            self.validate_integration_target(&capsule, &options.target)?;
        let result = self.store.read_result(id)?;
        let patch = self.store.read_patch(id)?;
        let (ignored_paths, ignored_bytes) =
            self.ignored_usage_for_policy(&policy, &capsule.workspace_path)?;
        Self::enforce_result_policy(
            &policy,
            patch.len() as u64,
            result.changed_paths.len(),
            ignored_paths,
            ignored_bytes,
        )?;
        self.start_integration(
            &mut capsule,
            &target,
            &target_before,
            target_head_ref,
            options,
        )?;

        let proposed_head = match self.prepare_integration(&capsule, &target, &result, &patch) {
            Ok(head) => head,
            Err(error) => {
                self.abort_integration(&mut capsule, &error)?;
                return Err(error);
            }
        };
        if proposed_head != target_before {
            self.git
                .create_ref(&target.worktree, &integration_ref(&capsule), &proposed_head)?;
        }
        capsule
            .integration
            .as_mut()
            .ok_or_else(|| Error::UnsafeState("integration record disappeared".to_owned()))?
            .target_head_after = Some(proposed_head.clone());
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(&capsule)?;

        let current_target = self.git.repository(&target.worktree)?;
        if current_target.common_dir != capsule.repository_common_dir
            || current_target.git_dir != target.git_dir
            || self.git.head_ref(&current_target.worktree)?
                != capsule
                    .integration
                    .as_ref()
                    .ok_or_else(|| Error::UnsafeState("integration record disappeared".to_owned()))?
                    .target_head_ref
            || !self.git.clean(&current_target.worktree)?
            || self.git.head(&current_target.worktree)? != target_before
        {
            return Err(Error::ForeignWorktree(target.worktree));
        }
        if proposed_head != target_before {
            self.git.fast_forward(&target.worktree, &proposed_head)?;
        }

        let integrated_at = now()?;
        let integration = capsule
            .integration
            .as_mut()
            .ok_or_else(|| Error::UnsafeState("integration record disappeared".to_owned()))?;
        integration.integrated_at_unix = Some(integrated_at);
        capsule.state = CapsuleState::Integrated;
        capsule.updated_at_unix = integrated_at;
        if proposed_head != target_before {
            self.git.delete_ref_if_matches(
                &target.worktree,
                &integration_ref(&capsule),
                &proposed_head,
            )?;
        }
        append_event(
            &mut capsule,
            AuditEventKind::Integrated,
            Some(CapsuleState::Integrating),
            CapsuleState::Integrated,
            BTreeMap::from([("target_head_after".to_owned(), proposed_head)]),
        )?;
        self.store.write_capsule(&capsule)?;
        Ok(capsule)
    }

    fn start_integration(
        &self,
        capsule: &mut Capsule,
        target: &Repository,
        target_before: &str,
        target_head_ref: String,
        options: &IntegrateOptions,
    ) -> Result<()> {
        let started_at = now()?;
        capsule.state = CapsuleState::Integrating;
        capsule.integration = Some(Integration {
            target_worktree: target.worktree.clone(),
            target_git_dir: target.git_dir.clone(),
            target_head_ref,
            target_head_before: target_before.to_owned(),
            target_head_after: None,
            commit_message: options.message.clone().unwrap_or_else(|| {
                capsule
                    .label
                    .as_ref()
                    .map_or_else(|| format!("Integrate capsule {}", capsule.id), Clone::clone)
            }),
            author_name: options.author.name.clone(),
            author_email: options.author.email.clone(),
            started_at_unix: started_at,
            integrated_at_unix: None,
        });
        capsule.updated_at_unix = started_at;
        append_event(
            capsule,
            AuditEventKind::IntegrationStarted,
            Some(CapsuleState::Closed),
            CapsuleState::Integrating,
            BTreeMap::from([
                (
                    "target_worktree_sha256".to_owned(),
                    sha256_hex(target.worktree.to_string_lossy().as_bytes()),
                ),
                ("target_head_before".to_owned(), target_before.to_owned()),
            ]),
        )?;
        self.store.write_capsule(capsule)
    }

    fn abort_integration(&self, capsule: &mut Capsule, error: &Error) -> Result<()> {
        capsule.state = CapsuleState::Closed;
        capsule.integration = None;
        capsule.updated_at_unix = now()?;
        append_event(
            capsule,
            AuditEventKind::IntegrationAborted,
            Some(CapsuleState::Integrating),
            CapsuleState::Closed,
            BTreeMap::from([("error".to_owned(), bounded_error(error))]),
        )?;
        self.store.write_capsule(capsule)
    }

    /// Remove a capsule's owned worktree and branch, keeping its durable record.
    ///
    /// Refuses paths that no longer prove they are the worktree this capsule
    /// created, even when `force` is set.
    pub fn drop_capsule(&self, id: &str, force: bool) -> Result<Capsule> {
        let mut capsule = self.show(id)?;
        let _global_lock = self.store.lock_global()?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        if capsule.state == CapsuleState::Dropped {
            return Ok(capsule);
        }
        if capsule.state == CapsuleState::Dropping {
            self.finish_cleanup(&mut capsule)?;
            capsule.updated_at_unix = now()?;
            append_event(
                &mut capsule,
                AuditEventKind::Recovered,
                Some(CapsuleState::Dropping),
                CapsuleState::Dropped,
                BTreeMap::from([(
                    "action".to_owned(),
                    "completed cleanup requested before restart".to_owned(),
                )]),
            )?;
            self.store.write_capsule(&capsule)?;
            return Ok(capsule);
        }
        if matches!(
            capsule.state,
            CapsuleState::Creating | CapsuleState::Checkpointing | CapsuleState::Integrating
        ) && !force
        {
            return Err(invalid_state(
                &capsule,
                "closed, integrated, orphaned, or --force",
            ));
        }
        if capsule.state == CapsuleState::Active && !force {
            return Err(Error::UnsealedChanges(id.to_owned()));
        }
        let previous_state = capsule.state;
        let require_sealed = matches!(
            capsule.state,
            CapsuleState::Closed | CapsuleState::Integrated
        ) && !force;
        if require_sealed {
            self.ensure_sealed(&capsule)?;
        }

        let execution_root = self.execution_root(&capsule)?;
        let branch_head = self.git.branch_head(&execution_root, &capsule.branch)?;
        if capsule.workspace_path.exists() {
            if capsule.workspace_git_dir.is_none() && capsule.state == CapsuleState::Creating {
                capsule.workspace_git_dir =
                    Some(self.git.repository(&capsule.workspace_path)?.git_dir);
            }
            self.validate_owned_worktree(&capsule)?;
            let workspace_head = self.git.head(&capsule.workspace_path)?;
            if branch_head.as_deref() != Some(workspace_head.as_str()) {
                return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
            }
        } else if require_sealed {
            return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
        } else if let Some(record) = self.registered_record(&capsule)? {
            Self::validate_registered_record(&capsule, &record, branch_head.as_deref())?;
        } else if branch_head
            .as_deref()
            .is_some_and(|head| !recorded_capsule_head(&capsule, head))
        {
            return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
        }

        let started_at = now()?;
        capsule.state = CapsuleState::Dropping;
        capsule.cleanup = Some(Cleanup {
            branch_head,
            require_sealed,
            started_at_unix: started_at,
        });
        capsule.updated_at_unix = started_at;
        append_event(
            &mut capsule,
            AuditEventKind::CleanupStarted,
            Some(previous_state),
            CapsuleState::Dropping,
            BTreeMap::from([
                ("force".to_owned(), force.to_string()),
                ("require_sealed".to_owned(), require_sealed.to_string()),
            ]),
        )?;
        self.store.write_capsule(&capsule)?;

        self.finish_cleanup(&mut capsule)?;
        capsule.updated_at_unix = now()?;
        append_event(
            &mut capsule,
            AuditEventKind::Dropped,
            Some(CapsuleState::Dropping),
            CapsuleState::Dropped,
            BTreeMap::new(),
        )?;
        self.store.write_capsule(&capsule)?;
        Ok(capsule)
    }

    /// Reconcile interrupted lifecycle transitions.
    ///
    /// Completes only transitions whose worktree identity, refs, commit parents,
    /// patch digests, and journals all agree; anything ambiguous is left for
    /// explicit inspection. Safe to call at process startup.
    pub fn recover(&self) -> Result<Vec<RecoveryAction>> {
        let _global_lock = self.store.lock_global()?;
        let capsules = self.store.list_capsules()?;
        let mut actions = Vec::new();
        for listed in capsules {
            let _project_lock = self.store.lock_project(&listed.project_key)?;
            if let Some(action) = self.recover_capsule_locked(&listed.id)? {
                actions.push(action);
            }
        }
        Ok(actions)
    }

    /// Reconcile one known capsule without scanning unrelated state records.
    ///
    /// Uses the same global-then-project lock order and transition logic as
    /// [`Self::recover`], and rereads the capsule after both locks are held.
    pub fn recover_capsule(&self, id: &str) -> Result<Option<RecoveryAction>> {
        crate::state::validate_id(id)?;
        let _global_lock = self.store.lock_global()?;
        let listed = self.store.read_capsule(id)?;
        let _project_lock = self.store.lock_project(&listed.project_key)?;
        self.recover_capsule_locked(id)
    }

    fn recover_capsule_locked(&self, id: &str) -> Result<Option<RecoveryAction>> {
        let mut capsule = self.store.read_capsule(id)?;
        let previous = capsule.state;
        let action = match capsule.state {
            CapsuleState::Creating => Some(self.recover_creating(&mut capsule)?),
            CapsuleState::Checkpointing => self.recover_checkpointing(&mut capsule)?,
            CapsuleState::Active => self.recover_active(&mut capsule)?,
            CapsuleState::Integrating => self.recover_integrating(&mut capsule)?,
            CapsuleState::Dropping => Some(self.finish_cleanup(&mut capsule)?),
            CapsuleState::Closed
            | CapsuleState::Integrated
            | CapsuleState::Orphaned
            | CapsuleState::Dropped => None,
        };
        if let Some(action) = action {
            capsule.updated_at_unix = now()?;
            let recovered_state = capsule.state;
            append_event(
                &mut capsule,
                AuditEventKind::Recovered,
                Some(previous),
                recovered_state,
                BTreeMap::from([("action".to_owned(), action.clone())]),
            )?;
            self.store.write_capsule(&capsule)?;
            Ok(Some(RecoveryAction {
                capsule_id: capsule.id.clone(),
                previous_state: previous,
                state: capsule.state,
                action,
            }))
        } else {
            Ok(None)
        }
    }

    fn policy_violations(&self, policy: &Policy, capsules: &[Capsule]) -> Result<Vec<String>> {
        policy.validate()?;
        let mut violations = Vec::new();
        let capsule_count = capsules.len() as u64;
        let live_count = capsules
            .iter()
            .filter(|capsule| capsule.state != CapsuleState::Dropped)
            .count() as u64;
        check_limit(
            &mut violations,
            "capsule records",
            capsule_count,
            policy.max_capsules,
        );
        check_limit(
            &mut violations,
            "live capsules",
            live_count,
            policy.max_live_capsules,
        );
        if policy.max_state_bytes.is_some() {
            check_limit(
                &mut violations,
                "state bytes",
                self.store.state_bytes()?,
                policy.max_state_bytes,
            );
        }
        if policy.max_workspace_bytes.is_some() {
            check_limit(
                &mut violations,
                "workspace bytes",
                self.store.workspace_bytes()?,
                policy.max_workspace_bytes,
            );
        }
        let observed_at = now()?;
        for capsule in capsules {
            violations.extend(self.capsule_policy_violations(policy, capsule, observed_at));
        }
        Ok(violations)
    }

    fn capsule_policy_violations(
        &self,
        policy: &Policy,
        capsule: &Capsule,
        observed_at: u64,
    ) -> Vec<String> {
        let mut violations = Vec::new();
        if !repository_allowed(policy, &capsule.source_worktree) {
            violations.push(format!(
                "capsule {} repository is outside allowed roots: {}",
                capsule.id,
                capsule.source_worktree.display()
            ));
        }
        if capsule.state != CapsuleState::Dropped {
            if let Some(limit) = policy.max_capsule_age_seconds {
                let age = observed_at.saturating_sub(capsule.created_at_unix);
                if age > limit {
                    violations.push(format!(
                        "capsule {} age {age} seconds exceeds limit {limit}",
                        capsule.id
                    ));
                }
            }
        }
        if let Some(reference) = &capsule.result {
            self.sealed_capsule_policy_violations(policy, capsule, reference, &mut violations);
        } else {
            self.active_capsule_policy_violations(policy, capsule, &mut violations);
        }
        violations
    }

    fn sealed_capsule_policy_violations(
        &self,
        policy: &Policy,
        capsule: &Capsule,
        reference: &ResultRef,
        violations: &mut Vec<String>,
    ) {
        check_capsule_limit(
            violations,
            &capsule.id,
            "patch bytes",
            reference.patch_bytes,
            Some(policy.max_patch_bytes),
        );
        check_capsule_limit(
            violations,
            &capsule.id,
            "changed paths",
            reference.changed_paths as u64,
            policy.max_changed_paths,
        );
        let result = match self.sealed_artifacts(capsule) {
            Ok((true, result, _)) => result,
            Ok((false, _, _)) => {
                violations.push(format!(
                    "capsule {} sealed result artifacts do not match their manifest",
                    capsule.id
                ));
                return;
            }
            Err(error) => {
                violations.push(format!(
                    "capsule {} result cannot be inspected: {error}",
                    capsule.id
                ));
                return;
            }
        };
        if policy.max_ignored_paths.is_none() && policy.max_ignored_bytes.is_none() {
            return;
        }
        let (ignored_paths, ignored_bytes) = if capsule.workspace_path.exists() {
            if let Err(error) = self.validate_owned_worktree(capsule) {
                violations.push(format!(
                    "capsule {} workspace cannot be inspected: {error}",
                    capsule.id
                ));
                (result.ignored_paths.len() as u64, result.ignored_bytes)
            } else {
                match self.git.ignored_paths(&capsule.workspace_path) {
                    Ok(paths) => {
                        let bytes = if policy.max_ignored_bytes.is_some() {
                            match ignored_usage(&capsule.workspace_path, &paths) {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    violations.push(format!(
                                        "capsule {} ignored bytes cannot be inspected: {error}",
                                        capsule.id
                                    ));
                                    result.ignored_bytes
                                }
                            }
                        } else {
                            0
                        };
                        (paths.len() as u64, bytes)
                    }
                    Err(error) => {
                        violations.push(format!(
                            "capsule {} ignored paths cannot be inspected: {error}",
                            capsule.id
                        ));
                        (result.ignored_paths.len() as u64, result.ignored_bytes)
                    }
                }
            }
        } else {
            (result.ignored_paths.len() as u64, result.ignored_bytes)
        };
        check_capsule_limit(
            violations,
            &capsule.id,
            "ignored paths",
            ignored_paths,
            policy.max_ignored_paths,
        );
        check_capsule_limit(
            violations,
            &capsule.id,
            "ignored bytes",
            ignored_bytes,
            policy.max_ignored_bytes,
        );
    }

    fn active_capsule_policy_violations(
        &self,
        policy: &Policy,
        capsule: &Capsule,
        violations: &mut Vec<String>,
    ) {
        if !matches!(
            capsule.state,
            CapsuleState::Active | CapsuleState::Checkpointing
        ) {
            return;
        }
        if let Err(error) = self.validate_owned_worktree(capsule) {
            violations.push(format!(
                "capsule {} active result usage cannot be inspected: {error}",
                capsule.id
            ));
            return;
        }
        match self.snapshot(capsule) {
            Ok(snapshot) => {
                check_capsule_limit(
                    violations,
                    &capsule.id,
                    "patch bytes",
                    snapshot.patch.len() as u64,
                    Some(policy.max_patch_bytes),
                );
                check_capsule_limit(
                    violations,
                    &capsule.id,
                    "changed paths",
                    snapshot.changed_paths.len() as u64,
                    policy.max_changed_paths,
                );
            }
            Err(error) => violations.push(format!(
                "capsule {} active result usage cannot be inspected: {error}",
                capsule.id
            )),
        }
        if policy.max_ignored_paths.is_none() && policy.max_ignored_bytes.is_none() {
            return;
        }
        match self.git.ignored_paths(&capsule.workspace_path) {
            Ok(ignored) => {
                check_capsule_limit(
                    violations,
                    &capsule.id,
                    "ignored paths",
                    ignored.len() as u64,
                    policy.max_ignored_paths,
                );
                if policy.max_ignored_bytes.is_some() {
                    match ignored_usage(&capsule.workspace_path, &ignored) {
                        Ok(bytes) => check_capsule_limit(
                            violations,
                            &capsule.id,
                            "ignored bytes",
                            bytes,
                            policy.max_ignored_bytes,
                        ),
                        Err(error) => violations.push(format!(
                            "capsule {} ignored bytes cannot be inspected: {error}",
                            capsule.id
                        )),
                    }
                }
            }
            Err(error) => violations.push(format!(
                "capsule {} ignored paths cannot be inspected: {error}",
                capsule.id
            )),
        }
    }

    fn enforce_create_policy(
        &self,
        policy: &Policy,
        capsules: &[Capsule],
        repository: &Repository,
        unmaterialized_reservations: u64,
    ) -> Result<()> {
        policy.validate()?;
        if !repository_allowed(policy, &repository.worktree) {
            return Err(Error::PolicyViolation(format!(
                "repository is outside allowed roots: {}",
                repository.worktree.display()
            )));
        }
        let capsule_records = (capsules.len() as u64)
            .checked_add(unmaterialized_reservations)
            .ok_or_else(|| Error::PolicyViolation("capsule record count overflowed".to_owned()))?;
        enforce_next_limit("capsule records", capsule_records, policy.max_capsules)?;
        let live_capsules = (capsules
            .iter()
            .filter(|capsule| capsule.state != CapsuleState::Dropped)
            .count() as u64)
            .checked_add(unmaterialized_reservations)
            .ok_or_else(|| Error::PolicyViolation("live capsule count overflowed".to_owned()))?;
        enforce_next_limit("live capsules", live_capsules, policy.max_live_capsules)?;
        if policy.max_state_bytes.is_some() {
            enforce_limit(
                "state bytes",
                self.store.state_bytes()?,
                policy.max_state_bytes,
            )?;
        }
        if policy.max_workspace_bytes.is_some() {
            enforce_limit(
                "workspace bytes",
                self.store.workspace_bytes()?,
                policy.max_workspace_bytes,
            )?;
        }
        Ok(())
    }

    fn enforce_capsule_policy(&self, capsule: &Capsule) -> Result<Policy> {
        let policy = self.store.read_policy()?;
        if !repository_allowed(&policy, &capsule.source_worktree) {
            return Err(Error::PolicyViolation(format!(
                "capsule repository is outside allowed roots: {}",
                capsule.source_worktree.display()
            )));
        }
        if let Some(limit) = policy.max_capsule_age_seconds {
            let age = now()?.saturating_sub(capsule.created_at_unix);
            if age > limit {
                return Err(Error::PolicyViolation(format!(
                    "capsule age {age} seconds exceeds limit {limit}"
                )));
            }
        }
        if policy.max_state_bytes.is_some() {
            enforce_limit(
                "state bytes",
                self.store.state_bytes()?,
                policy.max_state_bytes,
            )?;
        }
        if policy.max_workspace_bytes.is_some() {
            enforce_limit(
                "workspace bytes",
                self.store.workspace_bytes()?,
                policy.max_workspace_bytes,
            )?;
        }
        Ok(policy)
    }

    /// Ignored-content usage is measured only when a policy limit makes it
    /// relevant; the byte walk uses file metadata rather than reading content.
    fn ignored_usage_for_policy(&self, policy: &Policy, workspace: &Path) -> Result<(usize, u64)> {
        if policy.max_ignored_paths.is_none() && policy.max_ignored_bytes.is_none() {
            return Ok((0, 0));
        }
        let paths = self.git.ignored_paths(workspace)?;
        let bytes = if policy.max_ignored_bytes.is_some() {
            ignored_usage(workspace, &paths)?
        } else {
            0
        };
        Ok((paths.len(), bytes))
    }

    /// Confirm both manifests a checkpoint persists still fit.
    ///
    /// A checkpoint writes the manifest twice: once carrying the journal before
    /// the branch moves, and once carrying the finished checkpoint afterwards.
    /// The journaled form is the larger of the two, so checking only the final
    /// form would still let the first write fail and strand the capsule.
    fn project_checkpoint(capsule: &Capsule, journal: &CheckpointJournal) -> Result<()> {
        if capsule.checkpoints.len() >= CHECKPOINT_COUNT_CAP {
            return Err(Error::InvalidInput(format!(
                "a capsule retains at most {CHECKPOINT_COUNT_CAP} checkpoints; close this capsule and start another"
            )));
        }
        let mut journaled = capsule.clone();
        journaled.state = CapsuleState::Checkpointing;
        journaled.checkpoint = Some(journal.clone());
        crate::state::ensure_manifest_capacity(&journaled)?;

        let mut completed = capsule.clone();
        completed.state = CapsuleState::Active;
        completed.checkpoint = None;
        completed.checkpoints.push(Checkpoint {
            commit: journal.head_after.clone(),
            message: journal.message.clone(),
            author_name: journal.author_name.clone(),
            author_email: journal.author_email.clone(),
            created_at_unix: journal.started_at_unix,
        });
        append_event(
            &mut completed,
            AuditEventKind::Checkpointed,
            Some(CapsuleState::Checkpointing),
            CapsuleState::Active,
            BTreeMap::from([("commit".to_owned(), journal.head_after.clone())]),
        )?;
        crate::state::ensure_manifest_capacity(&completed)
    }

    fn enforce_result_policy(
        policy: &Policy,
        patch_bytes: u64,
        changed_paths: usize,
        ignored_paths: usize,
        ignored_bytes: u64,
    ) -> Result<()> {
        enforce_limit("patch bytes", patch_bytes, Some(policy.max_patch_bytes))?;
        enforce_limit(
            "changed paths",
            changed_paths as u64,
            policy.max_changed_paths,
        )?;
        enforce_limit(
            "ignored paths",
            ignored_paths as u64,
            policy.max_ignored_paths,
        )?;
        enforce_limit("ignored bytes", ignored_bytes, policy.max_ignored_bytes)
    }

    fn status_for(&self, capsule: Capsule) -> Result<CapsuleStatus> {
        if capsule.state == CapsuleState::Dropped {
            return Ok(CapsuleStatus {
                capsule,
                health: CapsuleHealth::Dropped,
                head_commit: None,
                dirty: None,
                changed_paths: Vec::new(),
                ignored_paths: Vec::new(),
                commits_ahead: None,
                sealed: None,
            });
        }
        if capsule.state == CapsuleState::Creating {
            return Ok(CapsuleStatus {
                capsule,
                health: CapsuleHealth::IncompleteCreation,
                head_commit: None,
                dirty: None,
                changed_paths: Vec::new(),
                ignored_paths: Vec::new(),
                commits_ahead: None,
                sealed: None,
            });
        }
        if !capsule.workspace_path.exists() {
            return Ok(CapsuleStatus {
                capsule,
                health: CapsuleHealth::MissingWorktree,
                head_commit: None,
                dirty: None,
                changed_paths: Vec::new(),
                ignored_paths: Vec::new(),
                commits_ahead: None,
                sealed: None,
            });
        }
        if self.validate_owned_worktree(&capsule).is_err() {
            return Ok(CapsuleStatus {
                capsule,
                health: CapsuleHealth::ForeignWorktree,
                head_commit: None,
                dirty: None,
                changed_paths: Vec::new(),
                ignored_paths: Vec::new(),
                commits_ahead: None,
                sealed: None,
            });
        }
        let snapshot = self.snapshot(&capsule)?;
        let ignored_paths = self.git.ignored_paths(&capsule.workspace_path)?;
        let head = self.git.head(&capsule.workspace_path)?;
        let dirty = !self.git.clean(&capsule.workspace_path)?;
        let commits_ahead = self
            .git
            .commits_ahead(&capsule.workspace_path, &capsule.base_commit)?;
        let sealed = capsule
            .result
            .as_ref()
            .map(|_| self.seal_matches(&capsule, &snapshot, &head))
            .transpose()?;
        let health = if sealed == Some(false)
            && matches!(
                capsule.state,
                CapsuleState::Closed
                    | CapsuleState::Integrating
                    | CapsuleState::Integrated
                    | CapsuleState::Dropping
            ) {
            CapsuleHealth::DriftedAfterClose
        } else if capsule.state == CapsuleState::Creating {
            CapsuleHealth::IncompleteCreation
        } else if capsule.state == CapsuleState::Checkpointing {
            CapsuleHealth::IncompleteCheckpoint
        } else {
            CapsuleHealth::Healthy
        };
        Ok(CapsuleStatus {
            capsule,
            health,
            head_commit: Some(head),
            dirty: Some(dirty),
            changed_paths: snapshot.changed_paths,
            ignored_paths,
            commits_ahead: Some(commits_ahead),
            sealed,
        })
    }

    fn validate_integration_target(
        &self,
        capsule: &Capsule,
        path: &Path,
    ) -> Result<(Repository, String, String)> {
        let target = self.git.repository(path)?;
        if target.common_dir != capsule.repository_common_dir {
            return Err(Error::ForeignWorktree(target.worktree));
        }
        if target.worktree == capsule.workspace_path {
            return Err(Error::InvalidInput(
                "cannot integrate a capsule into its own workspace".to_owned(),
            ));
        }
        if self.git.sparse_checkout(&target.worktree)?
            || self.git.hidden_index_entries(&target.worktree)?
        {
            return Err(Error::InvalidInput(
                "integration target uses sparse-checkout, skip-worktree, or assume-unchanged entries; restore a full checkout first"
                    .to_owned(),
            ));
        }
        if !self.git.clean(&target.worktree)? {
            return Err(Error::DirtyIntegrationTarget(target.worktree));
        }
        let head = self.git.head(&target.worktree)?;
        let head_ref = self.git.head_ref(&target.worktree)?;
        if head != capsule.base_commit {
            return Err(Error::InvalidInput(format!(
                "integration target HEAD {head} does not equal pinned base {}; update or recreate the capsule explicitly",
                capsule.base_commit
            )));
        }
        Ok((target, head, head_ref))
    }

    fn prepare_integration(
        &self,
        capsule: &Capsule,
        target: &Repository,
        result: &CapsuleResult,
        patch: &[u8],
    ) -> Result<String> {
        if result.kind == ResultKind::NoChange {
            return self.git.head(&target.worktree);
        }
        let integration = capsule
            .integration
            .as_ref()
            .ok_or_else(|| Error::UnsafeState("integration record disappeared".to_owned()))?;
        let message = format!(
            "{}\n\nChange-Capsule: {}",
            integration.commit_message, capsule.id
        );
        let index = self.store.temporary_index(&capsule.id)?;
        let commit = self.git.commit_patch(&CommitPatch {
            worktree: &target.worktree,
            base: &capsule.base_commit,
            patch,
            index: index.path(),
            message: &message,
            name: &integration.author_name,
            email: &integration.author_email,
        })?;
        let parents = self.git.parents(&target.worktree, &commit)?;
        if parents != [capsule.base_commit.clone()] {
            return Err(Error::UnsafeState(format!(
                "prepared integration commit {commit} does not have exactly the pinned base as parent"
            )));
        }
        let prepared = self
            .git
            .commit_snapshot(&target.worktree, &capsule.base_commit, &commit)?;
        if prepared.patch != patch || prepared.changed_paths != result.changed_paths {
            return Err(Error::UnsafeState(
                "prepared integration commit does not reproduce the sealed result".to_owned(),
            ));
        }
        Ok(commit)
    }

    fn ensure_sealed(&self, capsule: &Capsule) -> Result<()> {
        capsule
            .result
            .as_ref()
            .ok_or_else(|| invalid_state(capsule, "a sealed result"))?;
        self.validate_owned_worktree(capsule)?;
        let snapshot = self.snapshot(capsule)?;
        let head = self.git.head(&capsule.workspace_path)?;
        if !self.seal_matches(capsule, &snapshot, &head)? {
            return Err(Error::ResultDrift(capsule.id.clone()));
        }
        Ok(())
    }

    fn seal_matches(
        &self,
        capsule: &Capsule,
        snapshot: &crate::git::Snapshot,
        head: &str,
    ) -> Result<bool> {
        let (artifacts_match, result, stored_patch) = match self.sealed_artifacts(capsule) {
            Ok(artifacts) => artifacts,
            Err(Error::ResultDrift(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        // The ignored-content inventory sealed at close is provenance, not a gate:
        // ignored files are exactly the content the repository declared irrelevant,
        // so their later churn must not invalidate an otherwise intact result.
        Ok(artifacts_match
            && result.head_commit == head
            && result.patch_sha256 == sha256_hex(&snapshot.patch)
            && result.patch_bytes == snapshot.patch.len() as u64
            && result.changed_paths == snapshot.changed_paths
            && stored_patch == snapshot.patch)
    }

    fn artifact_snapshot(&self, id: &str) -> Result<(ArtifactBundle, Vec<u8>, Vec<u8>)> {
        let capsule = self.show(id)?;
        if capsule.result.is_none() {
            return Err(invalid_state(&capsule, "a sealed result"));
        }
        let (matches, _, result_bytes, patch) = self.sealed_artifact_snapshot(&capsule)?;
        if !matches {
            return Err(Error::ResultDrift(capsule.id));
        }
        let result_digest = sha256_hex(&result_bytes);
        let patch_digest = sha256_hex(&patch);
        let bundle = ArtifactBundle {
            schema_version: BUNDLE_SCHEMA_VERSION,
            capsule_id: id.to_owned(),
            artifacts: vec![
                artifact_descriptor(
                    ArtifactKind::ResultManifest,
                    "result.json",
                    "application/json",
                    &self.store.capsule_dir(id)?.join("result.json"),
                    &result_digest,
                    result_bytes.len() as u64,
                )?,
                artifact_descriptor(
                    ArtifactKind::ResultPatch,
                    "result.patch",
                    "application/vnd.git.patch",
                    &self.store.capsule_dir(id)?.join("result.patch"),
                    &patch_digest,
                    patch.len() as u64,
                )?,
            ],
        };
        Ok((bundle, result_bytes, patch))
    }

    fn sealed_artifacts(&self, capsule: &Capsule) -> Result<(bool, CapsuleResult, Vec<u8>)> {
        let (matches, result, _, patch) = self.sealed_artifact_snapshot(capsule)?;
        Ok((matches, result, patch))
    }

    fn sealed_artifact_snapshot(
        &self,
        capsule: &Capsule,
    ) -> Result<(bool, CapsuleResult, Vec<u8>, Vec<u8>)> {
        let Some(reference) = capsule.result.as_ref() else {
            return Err(invalid_state(capsule, "a sealed result"));
        };
        let (result, result_bytes) = self
            .store
            .read_result_artifact(&capsule.id)
            .map_err(|error| artifact_error(&capsule.id, error))?;
        let stored_patch = self
            .store
            .read_patch(&capsule.id)
            .map_err(|error| artifact_error(&capsule.id, error))?;
        let stored_digest = sha256_hex(&stored_patch);
        let matches = reference.kind == result.kind
            && reference.head_commit == result.head_commit
            && reference.patch_sha256 == stored_digest
            && reference.patch_sha256 == result.patch_sha256
            && reference.result_sha256 == result_sha256(&result)?
            && reference.patch_bytes == stored_patch.len() as u64
            && reference.patch_bytes == result.patch_bytes
            && reference.changed_paths == result.changed_paths.len()
            && reference.sealed_at_unix == result.sealed_at_unix
            && result.schema_version == SCHEMA_VERSION
            && result.capsule_id == capsule.id
            && result.label == capsule.label
            && result.links == capsule.links
            && result.base_commit == capsule.base_commit
            && result.checkpoints == capsule.checkpoints
            && result.evidence == capsule.evidence
            && result.created_at_unix == capsule.created_at_unix;
        Ok((matches, result, result_bytes, stored_patch))
    }

    fn close_snapshot_transaction(&self, capsule: &Capsule) -> Result<CloseSnapshotTransaction> {
        let initial_ignored = ignored_content_inventory(
            &capsule.workspace_path,
            self.git.ignored_paths(&capsule.workspace_path)?,
        )?;
        #[cfg(test)]
        run_close_ignored_inventory_test_hook(&capsule.id);
        let initial_snapshot = self.snapshot(capsule)?;
        let initial_head = self.git.head(&capsule.workspace_path)?;
        let initial_clean = self.git.clean(&capsule.workspace_path)?;
        let snapshot = self.snapshot(capsule)?;
        let head = self.git.head(&capsule.workspace_path)?;
        let clean = self.git.clean(&capsule.workspace_path)?;
        let ignored = ignored_content_inventory(
            &capsule.workspace_path,
            self.git.ignored_paths(&capsule.workspace_path)?,
        )?;
        require_stable_close_snapshot(
            &initial_snapshot,
            &initial_head,
            initial_clean,
            &snapshot,
            &head,
            clean,
        )?;
        require_stable_ignored_content(&initial_ignored, &ignored)?;
        Ok(CloseSnapshotTransaction {
            clean,
            snapshot,
            head,
            ignored,
        })
    }

    fn snapshot(&self, capsule: &Capsule) -> Result<crate::git::Snapshot> {
        self.snapshot_against(capsule, &capsule.base_commit)
    }

    fn snapshot_against(&self, capsule: &Capsule, base: &str) -> Result<crate::git::Snapshot> {
        if self.git.sparse_checkout(&capsule.workspace_path)? {
            return Err(Error::InvalidInput(
                "a capsule workspace with sparse checkout enabled cannot produce a complete snapshot; disable sparse checkout first"
                    .to_owned(),
            ));
        }
        if self.git.dirty_submodules(&capsule.workspace_path)? {
            return Err(Error::InvalidInput(
                "dirty submodule worktrees cannot be represented by a top-level capsule patch; commit or clean them first"
                    .to_owned(),
            ));
        }
        let index = self.store.temporary_index(&capsule.id)?;
        self.git
            .snapshot(&capsule.workspace_path, base, index.path())
    }

    fn validate_owned_worktree(&self, capsule: &Capsule) -> Result<()> {
        let repository = self
            .git
            .repository(&capsule.workspace_path)
            .map_err(|_| Error::ForeignWorktree(capsule.workspace_path.clone()))?;
        if repository.common_dir != capsule.repository_common_dir
            || capsule.workspace_git_dir.as_ref() != Some(&repository.git_dir)
            || repository.worktree != canonical_existing(&capsule.workspace_path)?
            || self.git.branch(&capsule.workspace_path)? != capsule.branch
        {
            return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
        }
        let record = self
            .registered_record(capsule)?
            .ok_or_else(|| Error::ForeignWorktree(capsule.workspace_path.clone()))?;
        let head = self.git.head(&capsule.workspace_path)?;
        Self::validate_registered_record(capsule, &record, Some(&head))?;
        Ok(())
    }

    fn registered_record(&self, capsule: &Capsule) -> Result<Option<crate::git::WorktreeRecord>> {
        let execution_root = self.execution_root(capsule)?;
        let records = self.git.registered_worktrees(&execution_root)?;
        Ok(records
            .into_iter()
            .find(|record| same_path_existing_or_clean(&record.path, &capsule.workspace_path)))
    }

    fn validate_registered_record(
        capsule: &Capsule,
        record: &crate::git::WorktreeRecord,
        expected_head: Option<&str>,
    ) -> Result<()> {
        if record.branch.as_deref() != Some(capsule.branch.as_str())
            || record.bare
            || record.head.as_deref() != expected_head
        {
            return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
        }
        Ok(())
    }

    fn finish_cleanup(&self, capsule: &mut Capsule) -> Result<String> {
        let cleanup = capsule.cleanup.clone().ok_or_else(|| {
            Error::UnsafeState("dropping capsule has no cleanup journal".to_owned())
        })?;
        let execution_root = self.execution_root(capsule)?;
        let expected_head = cleanup.branch_head.as_deref();

        if capsule.workspace_path.exists() {
            self.validate_owned_worktree(capsule)?;
            let workspace_head = self.git.head(&capsule.workspace_path)?;
            if expected_head != Some(workspace_head.as_str()) {
                return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
            }
            if cleanup.require_sealed {
                self.ensure_sealed(capsule)?;
            }
            self.git
                .remove_worktree(&execution_root, &capsule.workspace_path, true)?;
        } else if let Some(record) = self.registered_record(capsule)? {
            Self::validate_registered_record(capsule, &record, expected_head)?;
            self.git.prune(&execution_root)?;
            if self.registered_record(capsule)?.is_some() {
                return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
            }
        }

        match expected_head {
            Some(head) => {
                self.git
                    .delete_branch_if_matches(&execution_root, &capsule.branch, head)?;
            }
            None => {
                if self
                    .git
                    .branch_head(&execution_root, &capsule.branch)?
                    .is_some()
                {
                    return Err(Error::UnsafeState(format!(
                        "refusing to delete branch {} created after cleanup began",
                        capsule.branch
                    )));
                }
            }
        }
        self.git.prune(&execution_root)?;
        for pending_ref in [checkpoint_ref(capsule), integration_ref(capsule)] {
            if let Some(commit) = self.git.ref_head(&execution_root, &pending_ref)? {
                self.git
                    .delete_ref_if_matches(&execution_root, &pending_ref, &commit)?;
            }
        }
        let dropped_at = now()?;
        capsule.state = CapsuleState::Dropped;
        capsule.checkpoint = None;
        capsule.cleanup = None;
        capsule.dropped_at_unix = Some(dropped_at);
        Ok("completed journaled worktree and branch cleanup".to_owned())
    }

    fn execution_root(&self, capsule: &Capsule) -> Result<PathBuf> {
        for path in [&capsule.source_worktree, &capsule.workspace_path] {
            if path.exists()
                && self
                    .git
                    .repository(path)
                    .is_ok_and(|repository| repository.common_dir == capsule.repository_common_dir)
            {
                return Ok(path.clone());
            }
        }
        if capsule.repository_common_dir.exists()
            && canonical_existing(&capsule.repository_common_dir)? == capsule.repository_common_dir
        {
            return Ok(capsule.repository_common_dir.clone());
        }
        Err(Error::ForeignWorktree(capsule.workspace_path.clone()))
    }

    fn recover_creating(&self, capsule: &mut Capsule) -> Result<String> {
        // Recovery reconciles an identity that already exists on disk, so an
        // unprovable Git state must orphan it rather than fail the whole sweep.
        self.complete_creating(capsule, true)
    }

    fn recover_checkpointing(&self, capsule: &mut Capsule) -> Result<Option<String>> {
        Ok(self
            .finish_checkpoint(capsule)?
            .map(|checkpoint| format!("completed interrupted checkpoint {}", checkpoint.commit)))
    }

    fn finish_checkpoint(&self, capsule: &mut Capsule) -> Result<Option<Checkpoint>> {
        let journal = capsule.checkpoint.clone().ok_or_else(|| {
            Error::UnsafeState("checkpointing capsule has no checkpoint journal".to_owned())
        })?;
        self.validate_owned_worktree(capsule)?;
        let parents = self
            .git
            .parents(&capsule.workspace_path, &journal.head_after)?;
        let committed = self.git.commit_snapshot(
            &capsule.workspace_path,
            &journal.head_before,
            &journal.head_after,
        )?;
        if parents != [journal.head_before.clone()]
            || sha256_hex(&committed.patch) != journal.patch_sha256
        {
            return Err(Error::UnsafeState(
                "checkpoint journal does not match its prepared commit".to_owned(),
            ));
        }

        let head = self.git.head(&capsule.workspace_path)?;
        let pending_ref = checkpoint_ref(capsule);
        let pending_head = self.git.ref_head(&capsule.workspace_path, &pending_ref)?;
        if head == journal.head_before {
            match pending_head.as_deref() {
                Some(head) if head == journal.head_after => {}
                None => self.git.create_ref(
                    &capsule.workspace_path,
                    &pending_ref,
                    &journal.head_after,
                )?,
                Some(_) => {
                    return Err(Error::UnsafeState(
                        "checkpoint pending ref points to an unexpected commit".to_owned(),
                    ));
                }
            }
            self.git.advance_branch(
                &capsule.workspace_path,
                &capsule.branch,
                &journal.head_after,
                &journal.head_before,
            )?;
        } else if head == journal.head_after {
            if pending_head.as_deref().is_some_and(|value| value != head) {
                return Err(Error::UnsafeState(
                    "checkpoint pending ref points to an unexpected commit".to_owned(),
                ));
            }
            self.git
                .reset_index(&capsule.workspace_path, &journal.head_after)?;
        } else {
            return Ok(None);
        }
        if pending_head.is_some() {
            self.git.delete_ref_if_matches(
                &capsule.workspace_path,
                &pending_ref,
                &journal.head_after,
            )?;
        }

        if capsule
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.commit == journal.head_after)
        {
            return Err(Error::UnsafeState(
                "checkpoint journal duplicates an existing checkpoint".to_owned(),
            ));
        }
        let checkpoint = Checkpoint {
            commit: journal.head_after,
            message: journal.message,
            author_name: journal.author_name,
            author_email: journal.author_email,
            created_at_unix: journal.started_at_unix,
        };
        capsule.checkpoints.push(checkpoint.clone());
        capsule.checkpoint = None;
        capsule.state = CapsuleState::Active;
        Ok(Some(checkpoint))
    }

    fn recover_active(&self, capsule: &mut Capsule) -> Result<Option<String>> {
        if capsule.workspace_path.exists() && self.validate_owned_worktree(capsule).is_ok() {
            let pending_ref = checkpoint_ref(capsule);
            if let Some(commit) = self.git.ref_head(&capsule.workspace_path, &pending_ref)? {
                self.git
                    .delete_ref_if_matches(&capsule.workspace_path, &pending_ref, &commit)?;
                return Ok(Some(
                    "removed a prepared checkpoint ref left before its journal was written"
                        .to_owned(),
                ));
            }
            return Ok(None);
        }
        capsule.state = CapsuleState::Orphaned;
        Ok(Some(
            "marked an active capsule orphaned because its owned worktree is missing or foreign"
                .to_owned(),
        ))
    }

    fn integration_matches_result(
        &self,
        target: &Repository,
        integration: &Integration,
        result: &CapsuleResult,
        stored_patch: &[u8],
        head: &str,
    ) -> Result<bool> {
        if head == integration.target_head_before {
            return Ok(result.kind == ResultKind::NoChange
                && result.patch_bytes == 0
                && result.changed_paths.is_empty()
                && stored_patch.is_empty());
        }
        let parents = self.git.parents(&target.worktree, head)?;
        let snapshot =
            self.git
                .commit_snapshot(&target.worktree, &integration.target_head_before, head)?;
        Ok(parents == [integration.target_head_before.clone()]
            && snapshot.patch == stored_patch
            && snapshot.changed_paths == result.changed_paths)
    }

    fn recovery_integration_target(
        &self,
        capsule: &Capsule,
        integration: &Integration,
    ) -> Result<Option<Repository>> {
        if !integration.target_worktree.exists() {
            return Ok(None);
        }
        let Ok(target) = self.git.repository(&integration.target_worktree) else {
            return Ok(None);
        };
        if target.common_dir != capsule.repository_common_dir
            || target.git_dir != integration.target_git_dir
            || self.git.head_ref(&target.worktree)? != integration.target_head_ref
            || self.git.sparse_checkout(&target.worktree)?
            || self.git.hidden_index_entries(&target.worktree)?
            || !self.git.clean(&target.worktree)?
        {
            return Ok(None);
        }
        Ok(Some(target))
    }

    fn recover_integrating(&self, capsule: &mut Capsule) -> Result<Option<String>> {
        let Some(integration) = capsule.integration.clone() else {
            capsule.state = CapsuleState::Orphaned;
            return Ok(Some(
                "marked an integration orphaned because its journal record is missing".to_owned(),
            ));
        };
        let Some(target) = self.recovery_integration_target(capsule, &integration)? else {
            return Ok(None);
        };
        let head = self.git.head(&target.worktree)?;
        let pending_ref = integration_ref(capsule);
        let pending_ref_head = self.git.ref_head(&target.worktree, &pending_ref)?;
        if integration.target_head_after.is_none() {
            if head != integration.target_head_before {
                return Ok(None);
            }
            if let Some(prepared) = pending_ref_head {
                self.git
                    .delete_ref_if_matches(&target.worktree, &pending_ref, &prepared)?;
            }
            capsule.state = CapsuleState::Closed;
            capsule.integration = None;
            return Ok(Some(
                "removed a prepared integration ref left before its journal update".to_owned(),
            ));
        }
        let (artifacts_match, result, stored_patch) = self.sealed_artifacts(capsule)?;
        if !artifacts_match {
            return Ok(None);
        }
        let pending = integration
            .target_head_after
            .as_deref()
            .filter(|head| *head != integration.target_head_before);
        if let Some(head) = pending {
            if self
                .git
                .ref_head(&target.worktree, &pending_ref)?
                .is_some_and(|value| value != head)
            {
                return Err(Error::UnsafeState(
                    "integration pending ref points to an unexpected commit".to_owned(),
                ));
            }
        }
        if integration.target_head_after.as_deref() == Some(head.as_str()) {
            if !self.integration_matches_result(
                &target,
                &integration,
                &result,
                &stored_patch,
                &head,
            )? {
                return Ok(None);
            }
            let completed_at = now()?;
            if let Some(record) = capsule.integration.as_mut() {
                record.integrated_at_unix = Some(completed_at);
            }
            capsule.state = CapsuleState::Integrated;
            if let Some(pending) = pending {
                self.git
                    .delete_ref_if_matches(&target.worktree, &pending_ref, pending)?;
            }
            return Ok(Some(
                "finalized an integration whose exact Git commit completed before the journal update"
                    .to_owned(),
            ));
        }
        if head == integration.target_head_before {
            if let Some(pending) = pending {
                if self
                    .git
                    .ref_head(&target.worktree, &pending_ref)?
                    .as_deref()
                    != Some(pending)
                {
                    return Ok(None);
                }
            }
            capsule.state = CapsuleState::Closed;
            capsule.integration = None;
            if let Some(pending) = pending {
                self.git
                    .delete_ref_if_matches(&target.worktree, &pending_ref, pending)?;
            }
            return Ok(Some(
                "restored a pre-side-effect interrupted integration to closed".to_owned(),
            ));
        }
        Ok(None)
    }
}

fn artifact_descriptor(
    kind: ArtifactKind,
    name: &str,
    media_type: &str,
    path: &Path,
    digest: &str,
    bytes: u64,
) -> Result<ArtifactDescriptor> {
    Ok(ArtifactDescriptor {
        kind,
        name: name.to_owned(),
        media_type: media_type.to_owned(),
        uri: file_uri(path)?,
        content_address: format!("sha256:{digest}"),
        sha256: digest.to_owned(),
        bytes,
    })
}

fn file_uri(path: &Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| io(".", error))?
            .join(path)
    };
    let text = absolute
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path(absolute.clone()))?;
    #[cfg(windows)]
    let (prefix, normalized) = {
        let normalized = text.replace('\\', "/");
        if normalized.starts_with("//") {
            ("file:", normalized)
        } else {
            ("file:///", normalized)
        }
    };
    #[cfg(not(windows))]
    let (prefix, normalized) = ("file://", text.to_owned());
    let mut encoded = String::with_capacity(normalized.len() + prefix.len());
    encoded.push_str(prefix);
    for byte in normalized.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}")
                .map_err(|_| Error::UnsafeState("failed to encode artifact URI".to_owned()))?;
        }
    }
    Ok(encoded)
}

fn bounded_error(error: &Error) -> String {
    error
        .to_string()
        .chars()
        .take(512)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn append_event(
    capsule: &mut Capsule,
    kind: AuditEventKind,
    previous_state: Option<CapsuleState>,
    state: CapsuleState,
    attributes: BTreeMap<String, String>,
) -> Result<()> {
    let occurred_at_unix = now()?.max(
        capsule
            .audit_events
            .last()
            .map_or(capsule.created_at_unix, |event| event.occurred_at_unix),
    );
    if capsule.audit_events.len() >= AUDIT_EVENT_CAP {
        capsule.audit_events.remove(0);
        capsule.audit_events_dropped = capsule.audit_events_dropped.saturating_add(1);
    }
    capsule.audit_events.push(AuditEvent {
        schema_version: AUDIT_SCHEMA_VERSION,
        event_id: format!("evt-{}", Ulid::new().to_string().to_ascii_lowercase()),
        occurred_at_unix,
        kind,
        capsule_id: Some(capsule.id.clone()),
        project_key: Some(capsule.project_key.clone()),
        previous_state,
        state: Some(state),
        attributes,
    });
    Ok(())
}

fn state_name(state: CapsuleState) -> &'static str {
    match state {
        CapsuleState::Creating => "creating",
        CapsuleState::Checkpointing => "checkpointing",
        CapsuleState::Active => "active",
        CapsuleState::Closed => "closed",
        CapsuleState::Integrating => "integrating",
        CapsuleState::Integrated => "integrated",
        CapsuleState::Dropping => "dropping",
        CapsuleState::Orphaned => "orphaned",
        CapsuleState::Dropped => "dropped",
    }
}

fn repository_allowed(policy: &Policy, repository: &Path) -> bool {
    policy.allowed_repository_roots.is_empty()
        || policy
            .allowed_repository_roots
            .iter()
            .any(|root| repository.starts_with(root))
}

fn enforce_limit(name: &str, observed: u64, limit: Option<u64>) -> Result<()> {
    if let Some(limit) = limit {
        if observed > limit {
            return Err(Error::PolicyViolation(format!(
                "{name} {observed} exceeds limit {limit}"
            )));
        }
    }
    Ok(())
}

fn enforce_next_limit(name: &str, observed: u64, limit: Option<u64>) -> Result<()> {
    if let Some(limit) = limit {
        let next = observed.saturating_add(1);
        if next > limit {
            return Err(Error::PolicyViolation(format!(
                "creating a capsule would make {name} {next}, exceeding limit {limit}"
            )));
        }
    }
    Ok(())
}

fn check_limit(violations: &mut Vec<String>, name: &str, observed: u64, limit: Option<u64>) {
    if let Some(limit) = limit {
        if observed > limit {
            violations.push(format!("{name} {observed} exceeds limit {limit}"));
        }
    }
}

fn check_capsule_limit(
    violations: &mut Vec<String>,
    id: &str,
    name: &str,
    observed: u64,
    limit: Option<u64>,
) {
    if let Some(limit) = limit {
        if observed > limit {
            violations.push(format!(
                "capsule {id} {name} {observed} exceeds limit {limit}"
            ));
        }
    }
}

fn safe_ignored_relative(ignored: &crate::model::GitPath) -> Result<PathBuf> {
    let relative = ignored.to_path_buf().ok_or_else(|| {
        Error::InvalidInput(format!(
            "ignored path cannot be represented here: {ignored}"
        ))
    })?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(Error::UnsafeState(format!(
            "Git returned an unsafe ignored path: {ignored}"
        )));
    }
    Ok(relative)
}

#[derive(Debug)]
struct CloseSnapshotTransaction {
    clean: bool,
    snapshot: crate::git::Snapshot,
    head: String,
    ignored: IgnoredContentInventory,
}

#[derive(Debug, PartialEq, Eq)]
struct IgnoredContentInventory {
    paths: Vec<crate::model::GitPath>,
    bytes: u64,
    content_sha256: String,
}

fn ignored_content_inventory(
    workspace: &Path,
    ignored_paths: Vec<crate::model::GitPath>,
) -> Result<IgnoredContentInventory> {
    let mut digest = Sha256::new();
    digest.update(b"change-capsule ignored-content inventory v2\0");
    let mut total = 0_u64;
    let mut seen = BTreeSet::new();
    for ignored in &ignored_paths {
        let relative = safe_ignored_relative(ignored)?;
        inventory_path(workspace, &relative, &mut seen, &mut total, &mut digest)?;
    }
    Ok(IgnoredContentInventory {
        paths: ignored_paths,
        bytes: total,
        content_sha256: hex::encode(digest.finalize()),
    })
}

fn ignored_usage(workspace: &Path, ignored_paths: &[crate::model::GitPath]) -> Result<u64> {
    let mut total = 0_u64;
    let mut seen = BTreeSet::new();
    for ignored in ignored_paths {
        let relative = safe_ignored_relative(ignored)?;
        usage_path(workspace, &relative, &mut seen, &mut total)?;
    }
    Ok(total)
}

fn usage_path(
    workspace: &Path,
    relative: &Path,
    seen: &mut BTreeSet<PathBuf>,
    total: &mut u64,
) -> Result<()> {
    if !seen.insert(relative.to_path_buf()) {
        return Ok(());
    }
    let path = workspace.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io(&path, error))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let mut entries = fs::read_dir(&path)
            .map_err(|error| io(&path, error))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| io(&path, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            usage_path(workspace, &relative.join(entry.file_name()), seen, total)?;
        }
        return Ok(());
    }
    *total = total
        .checked_add(metadata.len())
        .ok_or_else(|| Error::UnsafeState("ignored byte count overflowed".to_owned()))?;
    Ok(())
}

fn inventory_path(
    workspace: &Path,
    relative: &Path,
    seen: &mut BTreeSet<PathBuf>,
    total: &mut u64,
    digest: &mut Sha256,
) -> Result<()> {
    if !seen.insert(relative.to_path_buf()) {
        return Ok(());
    }
    let path = workspace.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io(&path, error))?;
    update_native_path(digest, b"relative", relative)?;
    if metadata.file_type().is_symlink() {
        digest.update(b"link");
        let target = fs::read_link(&path).map_err(|error| io(&path, error))?;
        update_native_path(digest, b"target", &target)?;
        *total = total
            .checked_add(metadata.len())
            .ok_or_else(|| Error::UnsafeState("ignored byte count overflowed".to_owned()))?;
        return Ok(());
    }
    if metadata.is_file() {
        let mut file = open_ignored_inventory_file(&path)?;
        let opened = file.metadata().map_err(|error| io(&path, error))?;
        if !opened_regular_file(&opened) || opened.len() != metadata.len() {
            return Err(Error::UnsafeState(format!(
                "ignored file changed while it was inspected: {}",
                path.display()
            )));
        }
        let expected = opened.len();
        digest.update(b"file");
        digest.update(expected.to_be_bytes());
        let mut observed = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| io(&path, error))?;
            if read == 0 {
                break;
            }
            observed = observed
                .checked_add(read as u64)
                .ok_or_else(|| Error::UnsafeState("ignored byte count overflowed".to_owned()))?;
            if observed > expected {
                return Err(Error::UnsafeState(format!(
                    "ignored file changed while it was inspected: {}",
                    path.display()
                )));
            }
            digest.update(&buffer[..read]);
        }
        if observed != expected
            || file.metadata().map_err(|error| io(&path, error))?.len() != expected
        {
            return Err(Error::UnsafeState(format!(
                "ignored file changed while it was inspected: {}",
                path.display()
            )));
        }
        *total = total
            .checked_add(observed)
            .ok_or_else(|| Error::UnsafeState("ignored byte count overflowed".to_owned()))?;
        return Ok(());
    }
    if metadata.is_dir() {
        digest.update(b"dir");
        let mut entries = fs::read_dir(&path)
            .map_err(|error| io(&path, error))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| io(&path, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            inventory_path(
                workspace,
                &relative.join(entry.file_name()),
                seen,
                total,
                digest,
            )?;
        }
        return Ok(());
    }
    Err(Error::UnsafeState(format!(
        "ignored path has an unsupported special-file type: {}",
        path.display()
    )))
}

fn opened_regular_file(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.is_file()
            && metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_file()
    }
}

fn open_ignored_inventory_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
        options.custom_flags(
            i32::try_from(flags.bits())
                .expect("O_NOFOLLOW | O_NONBLOCK fits platform custom_flags"),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path).map_err(|error| io(path, error))
}

#[cfg_attr(any(unix, windows), allow(clippy::unnecessary_wraps))]
fn update_native_path(digest: &mut Sha256, field: &[u8], path: &Path) -> Result<()> {
    digest.update((field.len() as u64).to_be_bytes());
    digest.update(field);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        digest.update(b"unix-bytes");
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        digest.update(b"windows-utf16le");
        digest.update((units.len() as u64).to_be_bytes());
        for unit in units {
            digest.update(unit.to_le_bytes());
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let value = path
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(path.to_path_buf()))?;
        digest.update(b"portable-utf8");
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
        Ok(())
    }
}

fn artifact_error(id: &str, error: Error) -> Error {
    match error {
        Error::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            Error::ResultDrift(id.to_owned())
        }
        Error::Json { .. } | Error::UnsafeState(_) | Error::SchemaVersion { .. } => {
            Error::ResultDrift(id.to_owned())
        }
        other => other,
    }
}

fn recorded_capsule_head(capsule: &Capsule, head: &str) -> bool {
    head == capsule.base_commit
        || capsule
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.commit == head)
        || capsule
            .result
            .as_ref()
            .is_some_and(|result| result.head_commit == head)
        || capsule.checkpoint.as_ref().is_some_and(|checkpoint| {
            checkpoint.head_before == head || checkpoint.head_after == head
        })
}

fn checkpoint_ref(capsule: &Capsule) -> String {
    format!("refs/change-capsule/{}/checkpoint", capsule.id)
}

fn integration_ref(capsule: &Capsule) -> String {
    format!("refs/change-capsule/{}/integration", capsule.id)
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdempotentCreateTestStage {
    AfterReservation,
    AfterManifest,
}

#[cfg(test)]
static IDEMPOTENT_CREATE_TEST_HOOK: std::sync::Mutex<Option<IdempotentCreateTestStage>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn run_idempotent_create_test_hook(stage: IdempotentCreateTestStage) -> Result<()> {
    let mut hook = IDEMPOTENT_CREATE_TEST_HOOK
        .lock()
        .expect("idempotent-create test hook lock");
    if hook.as_ref() == Some(&stage) {
        *hook = None;
        return Err(Error::UnsafeState(
            "injected idempotent creation interruption".to_owned(),
        ));
    }
    Ok(())
}

fn is_unchecked_worktree_shape(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let mut entries = fs::read_dir(path).map_err(|error| io(path, error))?;
    let Some(entry) = entries
        .next()
        .transpose()
        .map_err(|error| io(path, error))?
    else {
        return Ok(false);
    };
    if entry.file_name() != std::ffi::OsStr::new(".git")
        || entries
            .next()
            .transpose()
            .map_err(|error| io(path, error))?
            .is_some()
    {
        return Ok(false);
    }
    let git_metadata =
        fs::symlink_metadata(entry.path()).map_err(|error| io(entry.path(), error))?;
    Ok(!git_metadata.file_type().is_symlink() && git_metadata.is_file())
}

fn new_capsule_id() -> String {
    format!("cap-{}", Ulid::new().to_string().to_ascii_lowercase())
}

fn path_entry_exists_no_follow(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(path, error)),
    }
}

fn validate_reservation_capsule(record: &IdempotencyRecord, capsule: &Capsule) -> Result<()> {
    if capsule.id != record.capsule_id
        || capsule.source_worktree != record.source_worktree
        || capsule.repository_common_dir != record.repository_common_dir
        || capsule.project_key != record.project_key
        || capsule.base_commit != record.base_commit
        || capsule.label != record.label
        || capsule.links != record.links
        || capsule.created_at_unix != record.reserved_at_unix
    {
        return Err(Error::UnsafeState(format!(
            "idempotency reservation does not agree with capsule {}",
            record.capsule_id
        )));
    }
    Ok(())
}

fn validate_create_options(options: &CreateOptions) -> Result<()> {
    if let Some(label) = &options.label {
        validate_bounded_text(label, LABEL_CAP, "label", false)?;
    }
    if options.links.len() > LINKS_LIMIT {
        return Err(Error::InvalidInput(format!(
            "at most {LINKS_LIMIT} links are allowed"
        )));
    }
    for (key, value) in &options.links {
        let valid_key = !key.is_empty()
            && key.len() <= LINK_KEY_CAP
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !valid_key {
            return Err(Error::InvalidInput(format!("invalid link key: {key:?}")));
        }
        validate_bounded_text(value, LINK_VALUE_CAP, "link value", false)?;
    }
    Ok(())
}

fn validate_author(author: &Author) -> Result<()> {
    validate_bounded_text(&author.name, 256, "author name", false)?;
    validate_bounded_text(&author.email, 512, "author email", false)?;
    if !author.email.contains('@') {
        return Err(Error::InvalidInput(
            "author email must contain '@'".to_owned(),
        ));
    }
    Ok(())
}

fn validate_message(message: &str, label: &str) -> Result<()> {
    validate_bounded_text(message, MESSAGE_CAP, label, true)
}

fn validate_bounded_text(value: &str, cap: usize, label: &str, multiline: bool) -> Result<()> {
    if value.trim().is_empty() || value.len() > cap || value.contains('\0') {
        return Err(Error::InvalidInput(format!(
            "{label} must contain 1-{cap} bytes without NUL"
        )));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !(multiline && matches!(character, '\n' | '\t')))
    {
        return Err(Error::InvalidInput(format!(
            "{label} contains unsupported control characters"
        )));
    }
    Ok(())
}

fn require_state(capsule: &Capsule, expected: CapsuleState, label: &str) -> Result<()> {
    if capsule.state == expected {
        Ok(())
    } else {
        Err(invalid_state(capsule, label))
    }
}

fn invalid_state(capsule: &Capsule, expected: &str) -> Error {
    Error::InvalidState {
        id: capsule.id.clone(),
        state: format!("{:?}", capsule.state).to_ascii_lowercase(),
        expected: expected.to_owned(),
    }
}

fn not_found(id: &str, error: Error) -> Error {
    match error {
        Error::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            Error::NotFound(id.to_owned())
        }
        other => other,
    }
}

fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| Error::InvalidInput("system clock is before the Unix epoch".to_owned()))
}

fn require_stable_ignored_content(
    initial: &IgnoredContentInventory,
    final_inventory: &IgnoredContentInventory,
) -> Result<()> {
    if initial != final_inventory {
        return Err(Error::UnsafeState(
            "capsule ignored paths or content changed while close was finalizing; no artifacts were written"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone)]
struct CloseIgnoredInventoryTestHook {
    capsule_id: String,
    initial_inventory_captured: std::sync::Arc<std::sync::Barrier>,
    mutation_finished: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
static CLOSE_IGNORED_INVENTORY_TEST_HOOK: std::sync::Mutex<Option<CloseIgnoredInventoryTestHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn run_close_ignored_inventory_test_hook(capsule_id: &str) {
    let hook = CLOSE_IGNORED_INVENTORY_TEST_HOOK
        .lock()
        .expect("close ignored-inventory test hook lock")
        .clone();
    if let Some(hook) = hook.filter(|hook| hook.capsule_id == capsule_id) {
        hook.initial_inventory_captured.wait();
        hook.mutation_finished.wait();
    }
}

fn require_stable_close_snapshot(
    initial: &crate::git::Snapshot,
    initial_head: &str,
    initial_clean: bool,
    final_snapshot: &crate::git::Snapshot,
    final_head: &str,
    final_clean: bool,
) -> Result<()> {
    if initial.patch != final_snapshot.patch
        || initial.changed_paths != final_snapshot.changed_paths
        || initial_head != final_head
        || initial_clean != final_clean
    {
        return Err(Error::UnsafeState(
            "capsule tracked content, HEAD, or clean state changed while close was finalizing; no artifacts were written"
                .to_owned(),
        ));
    }
    Ok(())
}

fn result_sha256(result: &CapsuleResult) -> Result<String> {
    let bytes = serde_json::to_vec(result).map_err(|source| Error::Json {
        path: PathBuf::from("result digest"),
        source,
    })?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    crate::path::canonicalize(path).map_err(|error| io(path, error))
}

fn same_path_existing_or_clean(left: &Path, right: &Path) -> bool {
    match (
        crate::path::canonicalize(left),
        crate::path::canonicalize(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => clean_absolute(left) == clean_absolute(right),
    }
}

fn clean_absolute(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Barrier};

    use super::{
        CLOSE_IGNORED_INVENTORY_TEST_HOOK, CapsuleManager, CloseIgnoredInventoryTestHook,
        CloseOptions, CreateOptions, IDEMPOTENT_CREATE_TEST_HOOK, IdempotentCreateTestStage,
        require_stable_close_snapshot,
    };
    use crate::error::Error;
    use crate::git::Snapshot;
    use crate::idempotency::{IdempotencyStatus, key_sha256};
    use crate::model::{CapsuleState, GitPath};

    fn snapshot(patch: &[u8], paths: &[&str]) -> Snapshot {
        Snapshot {
            patch: patch.to_vec(),
            changed_paths: paths
                .iter()
                .map(|path| GitPath::Utf8((*path).to_owned()))
                .collect(),
        }
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    static IDEMPOTENT_CREATE_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup_repository() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let repository = temporary.path().join("repository");
        let state = temporary.path().join("state");
        fs::create_dir(&repository).expect("create repository");
        git(&repository, &["init", "-b", "main"]);
        git(&repository, &["config", "core.autocrlf", "false"]);
        fs::write(repository.join("tracked.txt"), b"base\n").expect("write tracked file");
        git(&repository, &["add", "."]);
        git(
            &repository,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-m",
                "initial",
            ],
        );
        (temporary, repository, state)
    }

    fn idempotent_options(repository: &Path) -> CreateOptions {
        CreateOptions {
            repository: repository.to_path_buf(),
            base: "HEAD".to_owned(),
            label: Some("idempotent crash recovery".to_owned()),
            links: BTreeMap::new(),
        }
    }

    #[test]
    fn idempotent_retry_resumes_reservation_only_and_manifest_only_windows() {
        let _serial = IDEMPOTENT_CREATE_TEST_SERIAL
            .lock()
            .expect("idempotent test serial lock");
        for stage in [
            IdempotentCreateTestStage::AfterReservation,
            IdempotentCreateTestStage::AfterManifest,
        ] {
            let (_temporary, repository, state) = setup_repository();
            let manager = CapsuleManager::open(&state).expect("manager");
            *IDEMPOTENT_CREATE_TEST_HOOK
                .lock()
                .expect("install idempotency hook") = Some(stage);
            assert!(matches!(
                manager.create_idempotent(idempotent_options(&repository), "crash:window"),
                Err(Error::UnsafeState(message)) if message.contains("injected")
            ));
            let before = manager
                .lookup_idempotency_key("crash:window")
                .expect("lookup interrupted reservation");
            let reserved_id = before.capsule_id.clone();
            match stage {
                IdempotentCreateTestStage::AfterReservation => {
                    assert_eq!(before.status, IdempotencyStatus::Reserved);
                    assert!(before.capsule.is_none());
                }
                IdempotentCreateTestStage::AfterManifest => {
                    assert_eq!(before.status, IdempotencyStatus::Materialized);
                    assert_eq!(
                        before.capsule.expect("creating capsule").state,
                        CapsuleState::Creating
                    );
                }
            }
            let resumed = manager
                .create_idempotent(idempotent_options(&repository), "crash:window")
                .expect("resume exact reservation");
            assert_eq!(resumed.id, reserved_id);
            assert_eq!(resumed.state, CapsuleState::Active);
            assert_eq!(manager.list().expect("one capsule").len(), 1);
        }
    }

    #[test]
    fn contradictory_git_state_orphans_reserved_identity_without_replacement() {
        let _serial = IDEMPOTENT_CREATE_TEST_SERIAL
            .lock()
            .expect("idempotent test serial lock");
        let (_temporary, repository, state) = setup_repository();
        let manager = CapsuleManager::open(&state).expect("manager");
        *IDEMPOTENT_CREATE_TEST_HOOK
            .lock()
            .expect("install idempotency hook") = Some(IdempotentCreateTestStage::AfterReservation);
        assert!(
            manager
                .create_idempotent(idempotent_options(&repository), "partial:git")
                .is_err()
        );
        let digest = key_sha256("partial:git").expect("key digest");
        let record = manager
            .store
            .read_idempotency_record(&digest)
            .expect("read reservation")
            .expect("reservation");
        manager
            .git
            .create_ref(
                &repository,
                &format!("refs/heads/capsule/{}", &record.capsule_id[4..]),
                &record.base_commit,
            )
            .expect("inject reserved branch only");
        let replay = manager
            .create_idempotent(idempotent_options(&repository), "partial:git")
            .expect("same identity becomes orphaned");
        assert_eq!(replay.id, record.capsule_id);
        assert_eq!(replay.state, CapsuleState::Orphaned);
        assert_eq!(manager.list().expect("one capsule").len(), 1);
        // Orphaning is a successful lifecycle transition, so it must leave a
        // record of why; otherwise the capsule is inexplicably stuck.
        let event = replay
            .audit_events
            .last()
            .expect("orphaning records an audit event");
        assert_eq!(event.state, Some(CapsuleState::Orphaned));
        assert_eq!(event.previous_state, Some(CapsuleState::Creating));
        assert!(
            event.attributes["reason"].contains("contradictory"),
            "{:?}",
            event.attributes
        );
        let again = manager
            .create_idempotent(idempotent_options(&repository), "partial:git")
            .expect("orphan replay");
        assert_eq!(again.id, replay.id);
        assert_eq!(again.state, CapsuleState::Orphaned);
        // A replay of an orphan must not append a second orphaning event.
        assert_eq!(again.audit_events.len(), replay.audit_events.len());
    }

    #[test]
    fn close_rejects_ignored_content_mutated_between_inventories() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let repository = temporary.path().join("repository");
        let state = temporary.path().join("state");
        fs::create_dir(&repository).expect("create repository");
        git(&repository, &["init", "-b", "main"]);
        git(&repository, &["config", "core.autocrlf", "false"]);
        fs::write(repository.join("tracked.txt"), b"base\n").expect("write tracked file");
        git(&repository, &["add", "."]);
        git(
            &repository,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-m",
                "initial",
            ],
        );

        let manager = CapsuleManager::open(&state).expect("open manager");
        let capsule = manager
            .create(CreateOptions {
                repository,
                base: "HEAD".to_owned(),
                label: Some("ignored close race".to_owned()),
                links: BTreeMap::new(),
            })
            .expect("create capsule");
        fs::write(capsule.workspace_path.join(".gitignore"), b"ignored.log\n")
            .expect("write ignore rule");
        let ignored = capsule.workspace_path.join("ignored.log");
        fs::write(&ignored, b"initial ignored content\n").expect("write ignored content");

        let initial_inventory_captured = Arc::new(Barrier::new(2));
        let mutation_finished = Arc::new(Barrier::new(2));
        *CLOSE_IGNORED_INVENTORY_TEST_HOOK
            .lock()
            .expect("install close test hook") = Some(CloseIgnoredInventoryTestHook {
            capsule_id: capsule.id.clone(),
            initial_inventory_captured: Arc::clone(&initial_inventory_captured),
            mutation_finished: Arc::clone(&mutation_finished),
        });
        let mutation = std::thread::spawn(move || {
            initial_inventory_captured.wait();
            fs::write(ignored, b"mutated ignored content\n").expect("mutate ignored content");
            mutation_finished.wait();
        });

        let close = manager.close(&capsule.id, CloseOptions::default());
        mutation.join().expect("ignored-content mutation thread");
        *CLOSE_IGNORED_INVENTORY_TEST_HOOK
            .lock()
            .expect("clear close test hook") = None;

        assert!(matches!(
            close,
            Err(Error::UnsafeState(message))
                if message.contains("ignored paths or content changed")
                    && message.contains("no artifacts were written")
        ));
        assert_eq!(
            manager.show(&capsule.id).expect("active capsule").state,
            crate::model::CapsuleState::Active
        );
        let capsule_state = state.join("capsules").join(&capsule.id);
        assert!(!capsule_state.join("result.patch").exists());
        assert!(!capsule_state.join("result.json").exists());
    }

    #[test]
    fn close_stability_requires_exact_patch_paths_and_head() {
        let original = snapshot(b"patch", &["a"]);
        assert!(
            require_stable_close_snapshot(&original, "head", true, &original, "head", true).is_ok()
        );
        for final_snapshot in [snapshot(b"other", &["a"]), snapshot(b"patch", &["b"])] {
            assert!(matches!(
                require_stable_close_snapshot(
                    &original,
                    "head",
                    true,
                    &final_snapshot,
                    "head",
                    true,
                ),
                Err(Error::UnsafeState(message)) if message.contains("no artifacts were written")
            ));
        }
        assert!(matches!(
            require_stable_close_snapshot(&original, "head", true, &original, "other", true),
            Err(Error::UnsafeState(_))
        ));
        assert!(matches!(
            require_stable_close_snapshot(&original, "head", true, &original, "head", false),
            Err(Error::UnsafeState(_))
        ));
    }
}
