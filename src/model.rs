use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleState {
    Creating,
    Active,
    Closed,
    Integrating,
    Integrated,
    Orphaned,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub commit: String,
    pub message: String,
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
    pub target_head_before: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_head_after: Option<String>,
    pub started_at_unix: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrated_at_unix: Option<u64>,
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
    pub workspace_path: PathBuf,
    pub project_key: String,
    pub branch: String,
    pub base_commit: String,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration: Option<Integration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_at_unix: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleResult {
    pub schema_version: u32,
    pub capsule_id: String,
    pub kind: ResultKind,
    pub base_commit: String,
    pub head_commit: String,
    pub patch_sha256: String,
    pub patch_bytes: u64,
    pub changed_paths: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub sealed_at_unix: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleHealth {
    Healthy,
    MissingWorktree,
    ForeignWorktree,
    DriftedAfterClose,
    IncompleteCreation,
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
