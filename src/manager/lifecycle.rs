//! Lifecycle transitions: checkpoint, evidence, close, integrate, drop.
//!
//! Part of the [`CapsuleManager`] implementation; see the parent module.

// This file is a continuation of the parent module's `impl CapsuleManager`,
// so it deliberately shares the parent's imports rather than duplicating a
// large, constantly churning list.
#[allow(clippy::wildcard_imports)]
use super::*;

impl CapsuleManager {
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
        self.validate_owned_worktree(&capsule)?;
        let head_before = self.git.head(&capsule.workspace_path)?;
        let checkpoint_snapshot = self.snapshot_against(&capsule, &head_before)?;
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
        Self::require_patch_within_hard_bound(result_snapshot.patch.len() as u64)?;
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
        self.store.write_capsule(&capsule)?;
        Ok(checkpoint)
    }

    /// Attach a verification record to an active capsule.
    ///
    /// [`EvidenceInput::Run`] executes the command in the capsule workspace and
    /// records what Capsule observed. [`EvidenceInput::Claim`] records a
    /// caller's assertion and runs nothing. Either way the record is bound to
    /// the complete patch as it stands once the work is done.
    ///
    /// Bounded to 64 records and 256 KiB of text per capsule.
    pub fn add_evidence(&self, id: &str, input: EvidenceInput) -> Result<Evidence> {
        let command = input.command_line();
        validate_bounded_text(&command, EVIDENCE_COMMAND_CAP, "evidence command", false)?;

        // An executed command runs without any lock held. A test suite can run
        // for minutes, and holding the global lock for that long would serialize
        // every other capsule in the state root; a harness running attempts in
        // parallel would degrade to running them one at a time.
        let (exit_code, executed, output_sha256, output_bytes, summary) = match input {
            EvidenceInput::Claim {
                exit_code, summary, ..
            } => {
                if let Some(summary) = &summary {
                    validate_bounded_text(summary, EVIDENCE_SUMMARY_CAP, "evidence summary", true)?;
                }
                (exit_code, false, None, None, summary)
            }
            EvidenceInput::Run {
                argv,
                summary,
                timeout,
            } => {
                if let Some(summary) = &summary {
                    validate_bounded_text(summary, EVIDENCE_SUMMARY_CAP, "evidence summary", true)?;
                }
                let capsule = self.show(id)?;
                require_state(&capsule, CapsuleState::Active, "active")?;
                self.validate_owned_worktree(&capsule)?;
                let execution = crate::exec::run(&capsule.workspace_path, &argv, timeout)?;
                // `tail_text` already bounds and sanitizes this, but validate
                // it on the same path as a caller-supplied summary so the
                // manifest invariant does not depend on a bound set elsewhere.
                let summary = match summary {
                    Some(summary) => Some(summary),
                    None => Some(execution.tail)
                        .filter(|tail| !tail.is_empty())
                        .filter(|tail| {
                            validate_bounded_text(tail, EVIDENCE_SUMMARY_CAP, "summary", true)
                                .is_ok()
                        }),
                };
                (
                    execution.exit_code,
                    true,
                    Some(execution.output_sha256),
                    Some(execution.output_bytes),
                    summary,
                )
            }
        };

        let mut capsule = self.show(id)?;
        let _global_lock = self.store.lock_global()?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Active, "active")?;
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
        let pending_bytes = command.len() + summary.as_ref().map_or(0, String::len);
        if stored_evidence_bytes.saturating_add(pending_bytes) > EVIDENCE_TOTAL_BYTES_CAP {
            return Err(Error::InvalidInput(format!(
                "total evidence payload would exceed the {EVIDENCE_TOTAL_BYTES_CAP}-byte capsule bound"
            )));
        }
        // Bind the record to the patch as it stands now. An executed command may
        // itself have changed the tree, and the binding must describe what would
        // be sealed after the run, not before it.
        let snapshot = self.snapshot(&capsule)?;
        let evidence = Evidence {
            command,
            exit_code,
            executed,
            output_sha256,
            output_bytes,
            summary,
            patch_sha256: sha256_hex(&snapshot.patch),
            recorded_at_unix: now()?,
        };
        // The byte caps above count raw input; JSON escaping can still inflate
        // it, so confirm the encoded manifest fits before recording anything.
        let mut projected = capsule.clone();
        projected.evidence.push(evidence.clone());
        crate::state::ensure_manifest_capacity(&projected)?;
        capsule.evidence.push(evidence.clone());
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(&capsule)?;
        Ok(evidence)
    }

    /// Refuse a patch larger than the hard buffering bound.
    fn require_patch_within_hard_bound(bytes: u64) -> Result<()> {
        if bytes > crate::model::HARD_PATCH_BYTES {
            return Err(Error::InvalidInput(format!(
                "patch is {bytes} bytes, exceeding the {} byte hard bound",
                crate::model::HARD_PATCH_BYTES
            )));
        }
        Ok(())
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
            && !capsule
                .evidence
                .iter()
                .any(|item| item.exit_code == 0 && item.patch_sha256 == digest)
        {
            return Err(Error::InvalidInput(
                "current successful evidence is required, but no successful record is bound to the exact final patch being sealed"
                    .to_owned(),
            ));
        }
        if options.require_executed_evidence
            && !capsule
                .evidence
                .iter()
                .any(|item| item.executed && item.exit_code == 0 && item.patch_sha256 == digest)
        {
            return Err(Error::InvalidInput(
                "executed evidence is required, but Capsule did not itself run a passing command against the exact final patch being sealed"
                    .to_owned(),
            ));
        }
        Self::require_patch_within_hard_bound(snapshot.patch.len() as u64)?;
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
        self.ensure_sealed(&capsule)?;
        let (target, target_before, target_head_ref) =
            self.validate_integration_target(&capsule, &options.target)?;
        let result = self.store.read_result(id)?;
        let patch = self.store.read_patch(id)?;
        Self::require_patch_within_hard_bound(patch.len() as u64)?;
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
        self.store.write_capsule(&capsule)?;
        Ok(capsule)
    }

    pub(super) fn start_integration(
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
        self.store.write_capsule(capsule)
    }

    pub(super) fn abort_integration(&self, capsule: &mut Capsule, _error: &Error) -> Result<()> {
        capsule.state = CapsuleState::Closed;
        capsule.integration = None;
        capsule.updated_at_unix = now()?;
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
        self.store.write_capsule(&capsule)?;

        self.finish_cleanup(&mut capsule)?;
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(&capsule)?;
        Ok(capsule)
    }

    /// Confirm both manifests a checkpoint persists still fit.
    ///
    /// A checkpoint writes the manifest twice: once carrying the journal before
    /// the branch moves, and once carrying the finished checkpoint afterwards.
    /// The journaled form is the larger of the two, so checking only the final
    /// form would still let the first write fail and strand the capsule.
    pub(super) fn project_checkpoint(capsule: &Capsule, journal: &CheckpointJournal) -> Result<()> {
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
        crate::state::ensure_manifest_capacity(&completed)
    }

    pub(super) fn validate_integration_target(
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

    pub(super) fn prepare_integration(
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

    pub(super) fn close_snapshot_transaction(
        &self,
        capsule: &Capsule,
    ) -> Result<CloseSnapshotTransaction> {
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

    pub(super) fn validate_owned_worktree(&self, capsule: &Capsule) -> Result<()> {
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

    pub(super) fn registered_record(
        &self,
        capsule: &Capsule,
    ) -> Result<Option<crate::git::WorktreeRecord>> {
        let execution_root = self.execution_root(capsule)?;
        let records = self.git.registered_worktrees(&execution_root)?;
        Ok(records
            .into_iter()
            .find(|record| same_path_existing_or_clean(&record.path, &capsule.workspace_path)))
    }

    pub(super) fn validate_registered_record(
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

    pub(super) fn finish_cleanup(&self, capsule: &mut Capsule) -> Result<String> {
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

    pub(super) fn execution_root(&self, capsule: &Capsule) -> Result<PathBuf> {
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
}
