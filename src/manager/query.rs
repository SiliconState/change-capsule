//! Read-only inspection: listing, status, diffs, and sealed results.
//!
//! Part of the [`CapsuleManager`] implementation; see the parent module.

// This file is a continuation of the parent module's `impl CapsuleManager`,
// so it deliberately shares the parent's imports rather than duplicating a
// large, constantly churning list.
#[allow(clippy::wildcard_imports)]
use super::*;

impl CapsuleManager {
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

    /// Summarize capsules, reporting unreadable records rather than failing.
    ///
    /// [`Self::list`] fails closed on the first malformed record, which is
    /// correct for exact counts but leaves an operator with no way to see the rest of
    /// a large state root. This returns everything that reads, plus a bounded
    /// description of everything that did not.
    pub fn list_reporting(&self) -> Result<CapsuleListing> {
        let _lock = self.store.lock_global()?;
        let (capsules, unreadable) = self.store.list_capsules_lenient()?;
        Ok(CapsuleListing {
            capsules: capsules.iter().map(CapsuleSummary::from).collect(),
            unreadable,
        })
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

    pub(super) fn status_for(&self, capsule: Capsule) -> Result<CapsuleStatus> {
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

    pub(super) fn ensure_sealed(&self, capsule: &Capsule) -> Result<()> {
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

    pub(super) fn seal_matches(
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

    pub(super) fn snapshot(&self, capsule: &Capsule) -> Result<crate::git::Snapshot> {
        self.snapshot_against(capsule, &capsule.base_commit)
    }

    pub(super) fn snapshot_against(
        &self,
        capsule: &Capsule,
        base: &str,
    ) -> Result<crate::git::Snapshot> {
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
}
