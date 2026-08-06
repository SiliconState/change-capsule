use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 3;
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const AUDIT_SCHEMA_VERSION: u32 = 1;
pub const AUDIT_EVENT_CAP: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleState {
    Creating,
    Checkpointing,
    Active,
    Closed,
    Integrating,
    Integrated,
    Dropping,
    Orphaned,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub commit: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub created_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub command: String,
    pub exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub recorded_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultRef {
    pub kind: ResultKind,
    pub head_commit: String,
    pub patch_sha256: String,
    pub result_sha256: String,
    pub patch_bytes: u64,
    pub changed_paths: usize,
    pub sealed_at_unix: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    NoChange,
    Commit,
    Patch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integration {
    pub target_worktree: PathBuf,
    pub target_git_dir: PathBuf,
    pub target_head_ref: String,
    pub target_head_before: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_head_after: Option<String>,
    pub commit_message: String,
    pub author_name: String,
    pub author_email: String,
    pub started_at_unix: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrated_at_unix: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointJournal {
    pub head_before: String,
    pub head_after: String,
    pub patch_sha256: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub started_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cleanup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_head: Option<String>,
    pub require_sealed: bool,
    pub started_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capsule {
    pub schema_version: u32,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    pub state: CapsuleState,
    pub source_worktree: PathBuf,
    pub repository_common_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_git_dir: Option<PathBuf>,
    pub workspace_path: PathBuf,
    pub project_key: String,
    pub branch: String,
    pub base_commit: String,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointJournal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit_events: Vec<AuditEvent>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub audit_events_dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration: Option<Integration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<Cleanup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_at_unix: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleResult {
    pub schema_version: u32,
    pub capsule_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    pub kind: ResultKind,
    pub base_commit: String,
    pub head_commit: String,
    pub patch_sha256: String,
    pub patch_bytes: u64,
    pub changed_paths: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub ignored_paths_complete: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ignored_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_content_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    pub created_at_unix: u64,
    pub sealed_at_unix: u64,
}

const fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(value: &bool) -> bool {
    *value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleHealth {
    Healthy,
    MissingWorktree,
    ForeignWorktree,
    DriftedAfterClose,
    IncompleteCreation,
    IncompleteCheckpoint,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleStatus {
    pub capsule: Capsule,
    pub health: CapsuleHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_ahead: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleSummary {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub state: CapsuleState,
    pub workspace_path: PathBuf,
    pub base_commit: String,
    pub updated_at_unix: u64,
}

impl From<&Capsule> for CapsuleSummary {
    fn from(capsule: &Capsule) -> Self {
        Self {
            id: capsule.id.clone(),
            label: capsule.label.clone(),
            state: capsule.state,
            workspace_path: capsule.workspace_path.clone(),
            base_commit: capsule.base_commit.clone(),
            updated_at_unix: capsule.updated_at_unix,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryAction {
    pub capsule_id: String,
    pub previous_state: CapsuleState,
    pub state: CapsuleState,
    pub action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    ResultManifest,
    ResultPatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    pub kind: ArtifactKind,
    pub name: String,
    pub media_type: String,
    pub uri: String,
    pub content_address: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactBundle {
    pub schema_version: u32,
    pub capsule_id: String,
    pub artifacts: Vec<ArtifactDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportReport {
    pub bundle: ArtifactBundle,
    pub output_directory: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    Created,
    Checkpointed,
    EvidenceAdded,
    Closed,
    IntegrationStarted,
    IntegrationAborted,
    Integrated,
    CleanupStarted,
    Dropped,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub occurred_at_unix: u64,
    pub kind: AuditEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_state: Option<CapsuleState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<CapsuleState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRecordInspection {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub has_result: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateInspection {
    pub state_root: PathBuf,
    pub supported_schema_version: u32,
    pub state_bytes: u64,
    pub records: Vec<StateRecordInspection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub observed_at_unix: u64,
    pub capsules: u64,
    pub live_capsules: u64,
    pub sealed_results: u64,
    pub result_patch_bytes: u64,
    pub state_bytes: u64,
    pub workspace_bytes: u64,
    pub audit_events: u64,
    pub audit_events_dropped: u64,
    pub states: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    pub from_version: u32,
    pub to_version: u32,
    pub migrated_capsules: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupReport>,
}
