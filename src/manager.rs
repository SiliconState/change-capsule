use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::error::{Error, Result, io};
use crate::git::{CommitPatch, Git, Repository};
use crate::model::{
    Capsule, CapsuleHealth, CapsuleResult, CapsuleState, CapsuleStatus, CapsuleSummary, Checkpoint,
    Evidence, Integration, RecoveryAction, ResultKind, ResultRef, SCHEMA_VERSION,
};
use crate::state::{StateStore, default_state_root};

const LABEL_CAP: usize = 256;
const LINK_KEY_CAP: usize = 64;
const LINK_VALUE_CAP: usize = 4096;
const MESSAGE_CAP: usize = 16 * 1024;
const EVIDENCE_COMMAND_CAP: usize = 16 * 1024;
const EVIDENCE_SUMMARY_CAP: usize = 64 * 1024;
const DEFAULT_AUTHOR_NAME: &str = "Change Capsule";
const DEFAULT_AUTHOR_EMAIL: &str = "capsule@localhost";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub name: String,
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

#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub repository: PathBuf,
    pub base: String,
    pub label: Option<String>,
    pub links: BTreeMap<String, String>,
}

impl CreateOptions {
    pub fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
            base: "HEAD".to_owned(),
            label: None,
            links: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckpointOptions {
    pub message: String,
    pub author: Author,
}

#[derive(Debug, Clone)]
pub struct EvidenceInput {
    pub command: String,
    pub exit_code: i32,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CloseOptions {
    pub require_successful_evidence: bool,
}

#[derive(Debug, Clone)]
pub struct IntegrateOptions {
    pub target: PathBuf,
    pub message: Option<String>,
    pub author: Author,
}

#[derive(Debug)]
pub struct CapsuleManager {
    store: StateStore,
    git: Git,
}

impl CapsuleManager {
    pub fn open_default() -> Result<Self> {
        Self::open(default_state_root()?)
    }

    pub fn open(state_root: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            store: StateStore::open(state_root.into())?,
            git: Git::discover()?,
        })
    }

    pub fn state_root(&self) -> &Path {
        self.store.root()
    }

    pub fn create(&self, options: CreateOptions) -> Result<Capsule> {
        validate_create_options(&options)?;
        let repository = self.git.repository(&options.repository)?;
        let base_commit = self
            .git
            .resolve_commit(&repository.worktree, &options.base)?;
        let project_key = project_key(&repository.common_dir)?;
        let _lock = self.store.lock_project(&project_key)?;
        let id = format!("cap-{}", Ulid::new().to_string().to_ascii_lowercase());
        let branch = format!("capsule/{}", &id[4..]);
        let workspace_path = self.store.workspace_path(&project_key, &id)?;
        self.store.prepare_capsule(&id, &project_key)?;
        let created_at = now()?;
        let mut capsule = Capsule {
            schema_version: SCHEMA_VERSION,
            id,
            label: options.label,
            links: options.links,
            state: CapsuleState::Creating,
            source_worktree: repository.worktree.clone(),
            repository_common_dir: repository.common_dir.clone(),
            workspace_path,
            project_key,
            branch,
            base_commit,
            created_at_unix: created_at,
            updated_at_unix: created_at,
            checkpoints: Vec::new(),
            evidence: Vec::new(),
            result: None,
            integration: None,
            closed_at_unix: None,
            dropped_at_unix: None,
        };
        self.store.write_capsule(&capsule)?;
        self.git.add_worktree(
            &repository.worktree,
            &capsule.workspace_path,
            &capsule.branch,
            &capsule.base_commit,
        )?;
        self.validate_owned_worktree(&capsule)?;
        capsule.state = CapsuleState::Active;
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(&capsule)?;
        Ok(capsule)
    }

    pub fn list(&self) -> Result<Vec<CapsuleSummary>> {
        let _lock = self.store.lock_global()?;
        Ok(self
            .store
            .list_capsules()?
            .iter()
            .map(CapsuleSummary::from)
            .collect())
    }

    pub fn show(&self, id: &str) -> Result<Capsule> {
        self.store
            .read_capsule(id)
            .map_err(|error| not_found(id, error))
    }

    pub fn workspace_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self.show(id)?.workspace_path)
    }

    pub fn status(&self, id: &str) -> Result<CapsuleStatus> {
        let capsule = self.show(id)?;
        self.status_for(capsule)
    }

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
            CapsuleState::Dropped if capsule.result.is_some() => self
                .sealed_artifacts(&capsule)
                .and_then(|(matches, _, patch)| {
                    if matches {
                        Ok(patch)
                    } else {
                        Err(Error::ResultDrift(capsule.id.clone()))
                    }
                }),
            CapsuleState::Active | CapsuleState::Creating | CapsuleState::Orphaned => {
                let snapshot = self.snapshot(&capsule)?;
                Ok(snapshot.patch)
            }
            CapsuleState::Dropped => Ok(Vec::new()),
        }
    }

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

    pub fn result_patch_path(&self, id: &str) -> Result<PathBuf> {
        let capsule = self.show(id)?;
        if capsule.result.is_none() {
            return Err(invalid_state(&capsule, "a sealed result"));
        }
        Ok(self.store.capsule_dir(id)?.join("result.patch"))
    }

    pub fn checkpoint(&self, id: &str, options: CheckpointOptions) -> Result<Checkpoint> {
        validate_message(&options.message, "checkpoint message")?;
        validate_author(&options.author)?;
        let mut capsule = self.show(id)?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Active, "active")?;
        self.validate_owned_worktree(&capsule)?;
        if self.git.clean(&capsule.workspace_path)? {
            return Err(Error::InvalidInput("nothing to checkpoint".to_owned()));
        }
        let snapshot = self.snapshot(&capsule)?;
        if snapshot.patch.is_empty() {
            return Err(Error::InvalidInput("nothing to checkpoint".to_owned()));
        }
        let commit = self.git.checkpoint(
            &capsule.workspace_path,
            &options.message,
            &options.author.name,
            &options.author.email,
        )?;
        let checkpoint = Checkpoint {
            commit,
            message: options.message,
            created_at_unix: now()?,
        };
        capsule.checkpoints.push(checkpoint.clone());
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(&capsule)?;
        Ok(checkpoint)
    }

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
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Active, "active")?;
        let evidence = Evidence {
            command: input.command,
            exit_code: input.exit_code,
            summary: input.summary,
            recorded_at_unix: now()?,
        };
        capsule.evidence.push(evidence.clone());
        capsule.updated_at_unix = now()?;
        self.store.write_capsule(&capsule)?;
        Ok(evidence)
    }

    pub fn close(&self, id: &str, options: CloseOptions) -> Result<CapsuleResult> {
        let mut capsule = self.show(id)?;
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
        let snapshot = self.snapshot(&capsule)?;
        let head = self.git.head(&capsule.workspace_path)?;
        let clean = self.git.clean(&capsule.workspace_path)?;
        let kind = if snapshot.patch.is_empty() {
            ResultKind::NoChange
        } else if clean {
            ResultKind::Commit
        } else {
            ResultKind::Patch
        };
        let sealed_at = now()?;
        let digest = sha256_hex(&snapshot.patch);
        let result = CapsuleResult {
            schema_version: SCHEMA_VERSION,
            capsule_id: capsule.id.clone(),
            kind,
            base_commit: capsule.base_commit.clone(),
            head_commit: head.clone(),
            patch_sha256: digest.clone(),
            patch_bytes: snapshot.patch.len() as u64,
            changed_paths: snapshot.changed_paths.clone(),
            evidence: capsule.evidence.clone(),
            sealed_at_unix: sealed_at,
        };
        self.store.write_patch(id, &snapshot.patch)?;
        self.store.write_result(id, &result)?;
        capsule.result = Some(ResultRef {
            kind,
            head_commit: head,
            patch_sha256: digest,
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

    pub fn integrate(&self, id: &str, options: &IntegrateOptions) -> Result<Capsule> {
        validate_author(&options.author)?;
        if let Some(message) = &options.message {
            validate_message(message, "integration message")?;
        }
        let mut capsule = self.show(id)?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        require_state(&capsule, CapsuleState::Closed, "closed")?;
        self.ensure_sealed(&capsule)?;
        let target = self.git.repository(&options.target)?;
        if target.common_dir != capsule.repository_common_dir {
            return Err(Error::ForeignWorktree(target.worktree));
        }
        if target.worktree == capsule.workspace_path {
            return Err(Error::InvalidInput(
                "cannot integrate a capsule into its own workspace".to_owned(),
            ));
        }
        if !self.git.clean(&target.worktree)? {
            return Err(Error::DirtyIntegrationTarget(target.worktree));
        }
        let target_before = self.git.head(&target.worktree)?;
        if target_before != capsule.base_commit {
            return Err(Error::InvalidInput(format!(
                "integration target HEAD {target_before} does not equal pinned base {}; update or recreate the capsule explicitly",
                capsule.base_commit
            )));
        }
        let result = self.store.read_result(id)?;
        let patch = self.store.read_patch(id)?;
        let started_at = now()?;
        capsule.state = CapsuleState::Integrating;
        capsule.integration = Some(Integration {
            target_worktree: target.worktree.clone(),
            target_head_before: target_before.clone(),
            target_head_after: None,
            started_at_unix: started_at,
            integrated_at_unix: None,
        });
        capsule.updated_at_unix = started_at;
        self.store.write_capsule(&capsule)?;

        let integration = self.integrate_result(&capsule, &target, &result, &patch, options);
        match integration {
            Ok(target_after) => {
                let integrated_at = now()?;
                let integration = capsule.integration.as_mut().ok_or_else(|| {
                    Error::UnsafeState("integration record disappeared".to_owned())
                })?;
                integration.target_head_after = Some(target_after);
                integration.integrated_at_unix = Some(integrated_at);
                capsule.state = CapsuleState::Integrated;
                capsule.updated_at_unix = integrated_at;
                self.store.write_capsule(&capsule)?;
                Ok(capsule)
            }
            Err(error) => {
                if self
                    .git
                    .reset_hard(&target.worktree, &target_before)
                    .is_ok()
                {
                    capsule.state = CapsuleState::Closed;
                    capsule.integration = None;
                    capsule.updated_at_unix = now()?;
                    self.store.write_capsule(&capsule)?;
                }
                Err(error)
            }
        }
    }

    pub fn drop_capsule(&self, id: &str, force: bool) -> Result<Capsule> {
        let mut capsule = self.show(id)?;
        let _lock = self.store.lock_project(&capsule.project_key)?;
        capsule = self.store.read_capsule(id)?;
        if capsule.state == CapsuleState::Dropped {
            return Ok(capsule);
        }
        if matches!(
            capsule.state,
            CapsuleState::Creating | CapsuleState::Integrating
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
        if matches!(
            capsule.state,
            CapsuleState::Closed | CapsuleState::Integrated
        ) && !force
        {
            self.ensure_sealed(&capsule)?;
        }

        if capsule.workspace_path.exists() {
            self.validate_owned_worktree(&capsule)?;
            let execution_root = Self::execution_root(&capsule)?;
            self.git
                .remove_worktree(&execution_root, &capsule.workspace_path, true)?;
            self.git.delete_branch(&execution_root, &capsule.branch)?;
            self.git.prune(&execution_root)?;
        } else if self.registered_record(&capsule)?.is_some() {
            return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
        }
        let dropped_at = now()?;
        capsule.state = CapsuleState::Dropped;
        capsule.dropped_at_unix = Some(dropped_at);
        capsule.updated_at_unix = dropped_at;
        self.store.write_capsule(&capsule)?;
        Ok(capsule)
    }

    pub fn recover(&self) -> Result<Vec<RecoveryAction>> {
        let _global_lock = self.store.lock_global()?;
        let capsules = self.store.list_capsules()?;
        let mut actions = Vec::new();
        for listed in capsules {
            let _project_lock = self.store.lock_project(&listed.project_key)?;
            let mut capsule = self.store.read_capsule(&listed.id)?;
            let previous = capsule.state;
            let action = match capsule.state {
                CapsuleState::Creating => Some(self.recover_creating(&mut capsule)?),
                CapsuleState::Active => self.recover_active(&mut capsule),
                CapsuleState::Integrating => self.recover_integrating(&mut capsule)?,
                CapsuleState::Closed
                | CapsuleState::Integrated
                | CapsuleState::Orphaned
                | CapsuleState::Dropped => None,
            };
            if let Some(action) = action {
                capsule.updated_at_unix = now()?;
                self.store.write_capsule(&capsule)?;
                actions.push(RecoveryAction {
                    capsule_id: capsule.id.clone(),
                    previous_state: previous,
                    state: capsule.state,
                    action,
                });
            }
        }
        Ok(actions)
    }

    fn status_for(&self, capsule: Capsule) -> Result<CapsuleStatus> {
        if capsule.state == CapsuleState::Dropped {
            return Ok(CapsuleStatus {
                capsule,
                health: CapsuleHealth::Dropped,
                head_commit: None,
                dirty: None,
                changed_paths: Vec::new(),
                commits_ahead: None,
                sealed: None,
            });
        }
        if !capsule.workspace_path.exists() {
            let health = if capsule.state == CapsuleState::Creating {
                CapsuleHealth::IncompleteCreation
            } else {
                CapsuleHealth::MissingWorktree
            };
            return Ok(CapsuleStatus {
                capsule,
                health,
                head_commit: None,
                dirty: None,
                changed_paths: Vec::new(),
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
                commits_ahead: None,
                sealed: None,
            });
        }
        let snapshot = self.snapshot(&capsule)?;
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
                CapsuleState::Closed | CapsuleState::Integrating | CapsuleState::Integrated
            ) {
            CapsuleHealth::DriftedAfterClose
        } else if capsule.state == CapsuleState::Creating {
            CapsuleHealth::IncompleteCreation
        } else {
            CapsuleHealth::Healthy
        };
        Ok(CapsuleStatus {
            capsule,
            health,
            head_commit: Some(head),
            dirty: Some(dirty),
            changed_paths: snapshot.changed_paths,
            commits_ahead: Some(commits_ahead),
            sealed,
        })
    }

    fn integrate_result(
        &self,
        capsule: &Capsule,
        target: &Repository,
        result: &CapsuleResult,
        patch: &[u8],
        options: &IntegrateOptions,
    ) -> Result<String> {
        if result.kind == ResultKind::NoChange {
            return self.git.head(&target.worktree);
        }
        let message = options.message.clone().unwrap_or_else(|| {
            capsule
                .label
                .as_ref()
                .map_or_else(|| format!("Integrate capsule {}", capsule.id), Clone::clone)
        });
        let message = format!("{message}\n\nChange-Capsule: {}", capsule.id);
        let index = self.store.temporary_index(&capsule.id)?;
        let commit = self.git.commit_patch(&CommitPatch {
            worktree: &target.worktree,
            base: &capsule.base_commit,
            patch,
            index: index.path(),
            message: &message,
            name: &options.author.name,
            email: &options.author.email,
        })?;
        self.git.reset_hard(&target.worktree, &commit)?;
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
        let (artifacts_match, result, stored_patch) = self.sealed_artifacts(capsule)?;
        Ok(artifacts_match
            && result.head_commit == head
            && result.patch_sha256 == sha256_hex(&snapshot.patch)
            && result.patch_bytes == snapshot.patch.len() as u64
            && result.changed_paths == snapshot.changed_paths
            && stored_patch == snapshot.patch)
    }

    fn sealed_artifacts(&self, capsule: &Capsule) -> Result<(bool, CapsuleResult, Vec<u8>)> {
        let Some(reference) = capsule.result.as_ref() else {
            return Err(invalid_state(capsule, "a sealed result"));
        };
        let result = self.store.read_result(&capsule.id)?;
        let stored_patch = self.store.read_patch(&capsule.id)?;
        let stored_digest = sha256_hex(&stored_patch);
        let matches = reference.kind == result.kind
            && reference.head_commit == result.head_commit
            && reference.patch_sha256 == stored_digest
            && reference.patch_sha256 == result.patch_sha256
            && reference.patch_bytes == stored_patch.len() as u64
            && reference.patch_bytes == result.patch_bytes
            && reference.changed_paths == result.changed_paths.len()
            && reference.sealed_at_unix == result.sealed_at_unix
            && result.schema_version == SCHEMA_VERSION
            && result.capsule_id == capsule.id
            && result.base_commit == capsule.base_commit
            && result.evidence == capsule.evidence;
        Ok((matches, result, stored_patch))
    }

    fn snapshot(&self, capsule: &Capsule) -> Result<crate::git::Snapshot> {
        let index = self.store.temporary_index(&capsule.id)?;
        self.git
            .snapshot(&capsule.workspace_path, &capsule.base_commit, index.path())
    }

    fn validate_owned_worktree(&self, capsule: &Capsule) -> Result<()> {
        let repository = self
            .git
            .repository(&capsule.workspace_path)
            .map_err(|_| Error::ForeignWorktree(capsule.workspace_path.clone()))?;
        if repository.common_dir != capsule.repository_common_dir
            || repository.worktree != canonical_existing(&capsule.workspace_path)?
            || self.git.branch(&capsule.workspace_path)? != capsule.branch
        {
            return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
        }
        let record = self
            .registered_record(capsule)?
            .ok_or_else(|| Error::ForeignWorktree(capsule.workspace_path.clone()))?;
        if record.branch.as_deref() != Some(capsule.branch.as_str()) || record.bare {
            return Err(Error::ForeignWorktree(capsule.workspace_path.clone()));
        }
        Ok(())
    }

    fn registered_record(&self, capsule: &Capsule) -> Result<Option<crate::git::WorktreeRecord>> {
        let execution_root = Self::execution_root(capsule)?;
        let records = self.git.registered_worktrees(&execution_root)?;
        Ok(records
            .into_iter()
            .find(|record| same_path_existing_or_clean(&record.path, &capsule.workspace_path)))
    }

    fn execution_root(capsule: &Capsule) -> Result<PathBuf> {
        if capsule.source_worktree.exists() {
            return Ok(capsule.source_worktree.clone());
        }
        if capsule.workspace_path.exists() {
            return Ok(capsule.workspace_path.clone());
        }
        Err(Error::ForeignWorktree(capsule.workspace_path.clone()))
    }

    fn recover_creating(&self, capsule: &mut Capsule) -> Result<String> {
        if capsule.workspace_path.exists() && self.validate_owned_worktree(capsule).is_ok() {
            self.git
                .reset_hard(&capsule.workspace_path, &capsule.base_commit)?;
            capsule.state = CapsuleState::Active;
            return Ok("completed an interrupted workspace creation".to_owned());
        }
        capsule.state = CapsuleState::Orphaned;
        Ok(
            "marked an incomplete creation orphaned for explicit inspection or forced cleanup"
                .to_owned(),
        )
    }

    fn recover_active(&self, capsule: &mut Capsule) -> Option<String> {
        if capsule.workspace_path.exists() && self.validate_owned_worktree(capsule).is_ok() {
            return None;
        }
        capsule.state = CapsuleState::Orphaned;
        Some(
            "marked an active capsule orphaned because its owned worktree is missing or foreign"
                .to_owned(),
        )
    }

    fn recover_integrating(&self, capsule: &mut Capsule) -> Result<Option<String>> {
        let Some(integration) = capsule.integration.clone() else {
            capsule.state = CapsuleState::Orphaned;
            return Ok(Some(
                "marked an integration orphaned because its journal record is missing".to_owned(),
            ));
        };
        if !integration.target_worktree.exists() {
            return Ok(None);
        }
        let target = self.git.repository(&integration.target_worktree)?;
        if target.common_dir != capsule.repository_common_dir
            || !self.git.clean(&target.worktree)?
        {
            return Ok(None);
        }
        let head = self.git.head(&target.worktree)?;
        if head == integration.target_head_before {
            capsule.state = CapsuleState::Closed;
            capsule.integration = None;
            return Ok(Some(
                "restored a pre-side-effect interrupted integration to closed".to_owned(),
            ));
        }
        let snapshot_capsule = Capsule {
            workspace_path: target.worktree.clone(),
            ..capsule.clone()
        };
        let snapshot = self.snapshot(&snapshot_capsule)?;
        let result = self.store.read_result(&capsule.id)?;
        if sha256_hex(&snapshot.patch) == result.patch_sha256 {
            let completed_at = now()?;
            if let Some(record) = capsule.integration.as_mut() {
                record.target_head_after = Some(head);
                record.integrated_at_unix = Some(completed_at);
            }
            capsule.state = CapsuleState::Integrated;
            return Ok(Some(
                "finalized an integration whose Git commit completed before the journal update"
                    .to_owned(),
            ));
        }
        Ok(None)
    }
}

fn validate_create_options(options: &CreateOptions) -> Result<()> {
    if let Some(label) = &options.label {
        validate_bounded_text(label, LABEL_CAP, "label", false)?;
    }
    if options.links.len() > 32 {
        return Err(Error::InvalidInput(
            "at most 32 links are allowed".to_owned(),
        ));
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

fn project_key(common_dir: &Path) -> Result<String> {
    let path = common_dir
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path(common_dir.to_path_buf()))?;
    let digest = Sha256::digest(path.as_bytes());
    Ok(hex::encode(&digest[..12]))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| io(path, error))
}

fn same_path_existing_or_clean(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
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
