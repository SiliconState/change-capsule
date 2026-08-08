//! Conservative reconciliation of interrupted journal transitions.
//!
//! Part of the [`CapsuleManager`] implementation; see the parent module.

// This file is a continuation of the parent module's `impl CapsuleManager`,
// so it deliberately shares the parent's imports rather than duplicating a
// large, constantly churning list.
#[allow(clippy::wildcard_imports)]
use super::*;

impl CapsuleManager {
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

    pub(super) fn recover_capsule_locked(&self, id: &str) -> Result<Option<RecoveryAction>> {
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

    pub(super) fn recover_creating(&self, capsule: &mut Capsule) -> Result<String> {
        // Recovery reconciles an identity that already exists on disk, so an
        // unprovable Git state must orphan it rather than fail the whole sweep.
        self.complete_creating(capsule, true)
    }

    pub(super) fn recover_checkpointing(&self, capsule: &mut Capsule) -> Result<Option<String>> {
        Ok(self
            .finish_checkpoint(capsule)?
            .map(|checkpoint| format!("completed interrupted checkpoint {}", checkpoint.commit)))
    }

    pub(super) fn finish_checkpoint(&self, capsule: &mut Capsule) -> Result<Option<Checkpoint>> {
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

    pub(super) fn recover_active(&self, capsule: &mut Capsule) -> Result<Option<String>> {
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

    pub(super) fn integration_matches_result(
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

    pub(super) fn recovery_integration_target(
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

    pub(super) fn recover_integrating(&self, capsule: &mut Capsule) -> Result<Option<String>> {
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
