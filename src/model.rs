//! Durable data types for capsules, sealed results, artifacts, and audit
//! records.
//!
//! Every type here is the on-disk and JSON representation of some part of an
//! attempt. Timestamps are seconds since the Unix epoch. Commit fields hold
//! full hexadecimal Git object IDs, and digest fields hold lowercase
//! hexadecimal SHA-256.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Schema version of the durable capsule manifest and sealed result.
///
/// State written by a different version fails closed rather than being
/// interpreted under current assumptions.
pub const SCHEMA_VERSION: u32 = 3;

/// Schema version of the exported artifact bundle (`bundle.json`).
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// Schema version of individual [`AuditEvent`] records.
pub const AUDIT_SCHEMA_VERSION: u32 = 1;

/// Maximum number of audit events retained in one capsule manifest.
///
/// Older events roll off and are counted by [`Capsule::audit_events_dropped`].
pub const AUDIT_EVENT_CAP: usize = 128;

/// Lifecycle position of a capsule.
///
/// `Creating`, `Checkpointing`, `Integrating`, and `Dropping` are journal
/// states: a process crash can leave a capsule in one of them, and
/// [`CapsuleManager::recover`](crate::CapsuleManager::recover) completes only
/// those transitions it can prove are safe to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleState {
    /// The workspace is being created and is not yet usable.
    Creating,
    /// A checkpoint commit is being prepared and journaled.
    Checkpointing,
    /// The capsule is usable and its workspace may be modified.
    Active,
    /// The result is sealed and the workspace should be treated as read-only.
    Closed,
    /// A sealed result is being applied to an integration target.
    Integrating,
    /// A sealed result was explicitly applied to a target worktree.
    Integrated,
    /// Cleanup has begun and is journaled.
    Dropping,
    /// Ownership of the workspace could not be proven; awaiting inspection.
    Orphaned,
    /// The owned worktree and branch are gone; the durable record remains.
    Dropped,
}

/// A commit created inside the capsule during the attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Git object ID of the checkpoint commit.
    pub commit: String,
    /// Commit message supplied by the caller.
    pub message: String,
    /// Author name recorded on the commit.
    pub author_name: String,
    /// Author email recorded on the commit.
    pub author_email: String,
    /// When the checkpoint was started.
    pub created_at_unix: u64,
}

/// A verification claim recorded by the caller.
///
/// Capsule never runs verification itself. Evidence is provenance about
/// what the caller says it ran, not a cryptographic attestation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Exact command the caller reports having run.
    pub command: String,
    /// Exit status the caller observed.
    pub exit_code: i32,
    /// Optional bounded human- or machine-generated summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// When the record was attached to the capsule.
    pub recorded_at_unix: u64,
}

/// Seal recorded in the capsule manifest that binds it to its sealed result.
///
/// Every field is cross-checked against the stored `result.json` and
/// `result.patch` before those artifacts are trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultRef {
    /// Shape of the sealed result.
    pub kind: ResultKind,
    /// Workspace `HEAD` at the moment of sealing.
    pub head_commit: String,
    /// SHA-256 of the sealed patch bytes.
    pub patch_sha256: String,
    /// SHA-256 of the serialized sealed result.
    pub result_sha256: String,
    /// Size of the sealed patch in bytes.
    pub patch_bytes: u64,
    /// Number of paths the sealed result changes.
    pub changed_paths: usize,
    /// When the result was sealed.
    pub sealed_at_unix: u64,
}

/// Shape of a sealed result relative to its pinned base.
///
/// All three carry an equally complete patch and integrate identically; the
/// distinction records how the attempt left its workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    /// The workspace matched the base exactly; the patch is empty.
    NoChange,
    /// The workspace was clean and every change was committed.
    Commit,
    /// Uncommitted work was included in the sealed patch.
    Patch,
}

/// Journal describing an explicit integration of a sealed result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integration {
    /// Worktree the result is applied to.
    pub target_worktree: PathBuf,
    /// Git administration directory of that worktree, captured before any change.
    pub target_git_dir: PathBuf,
    /// Symbolic `HEAD` of the target, such as `refs/heads/main` or `HEAD`.
    pub target_head_ref: String,
    /// Target `HEAD` before integration; always the capsule's pinned base.
    pub target_head_before: String,
    /// Commit the target was advanced to, once one has been prepared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_head_after: Option<String>,
    /// Message used for the integration commit.
    pub commit_message: String,
    /// Author name recorded on the integration commit.
    pub author_name: String,
    /// Author email recorded on the integration commit.
    pub author_email: String,
    /// When integration began.
    pub started_at_unix: u64,
    /// When integration completed, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrated_at_unix: Option<u64>,
}

/// Journal written before a checkpoint commit becomes reachable.
///
/// Recovery finishes the transition only when the prepared commit's parent,
/// patch digest, and protecting ref all agree with this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointJournal {
    /// Workspace `HEAD` before the checkpoint.
    pub head_before: String,
    /// Prepared checkpoint commit.
    pub head_after: String,
    /// SHA-256 of the patch the prepared commit must reproduce.
    pub patch_sha256: String,
    /// Message for the checkpoint commit.
    pub message: String,
    /// Author name for the checkpoint commit.
    pub author_name: String,
    /// Author email for the checkpoint commit.
    pub author_email: String,
    /// When the checkpoint was started.
    pub started_at_unix: u64,
}

/// Journal written before destructive cleanup begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cleanup {
    /// Capsule branch tip observed when cleanup started, if the branch existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_head: Option<String>,
    /// Whether cleanup must still prove the result seal before removing anything.
    pub require_sealed: bool,
    /// When cleanup began.
    pub started_at_unix: u64,
}

/// The durable record of one change attempt.
///
/// This is the manifest persisted as `capsule.json`. Identity fields are
/// revalidated on every read, so a manifest whose branch, workspace path, or
/// repository identity has been edited is rejected rather than acted on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capsule {
    /// Schema version of this record; always [`SCHEMA_VERSION`] when written.
    pub schema_version: u32,
    /// Collision-resistant identifier, formatted as `cap-<ulid>`.
    pub id: String,
    /// Optional human-facing description of the attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Opaque caller metadata, such as `task=issue-42`. No key is privileged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    /// Current lifecycle position.
    pub state: CapsuleState,
    /// Worktree the capsule was created from.
    pub source_worktree: PathBuf,
    /// Canonical Git common directory shared by the repository's worktrees.
    pub repository_common_dir: PathBuf,
    /// Git administration directory of the capsule's own worktree, once created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_git_dir: Option<PathBuf>,
    /// Ordinary filesystem directory the attempt works in.
    pub workspace_path: PathBuf,
    /// Truncated digest of the repository identity, used to group state.
    pub project_key: String,
    /// Branch checked out in the capsule workspace, formatted as `capsule/<ulid>`.
    pub branch: String,
    /// Immutable commit the attempt started from.
    pub base_commit: String,
    /// When the capsule was created.
    pub created_at_unix: u64,
    /// When the capsule was last mutated.
    pub updated_at_unix: u64,
    /// Checkpoint commits made during the attempt, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
    /// Journal for a checkpoint that has not finished transitioning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointJournal>,
    /// Verification claims recorded by the caller, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    /// Retained lifecycle events, newest last, capped at [`AUDIT_EVENT_CAP`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit_events: Vec<AuditEvent>,
    /// Count of audit events that rolled off the retained window.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub audit_events_dropped: u64,
    /// Seal binding this capsule to its result artifacts, once closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultRef>,
    /// Journal describing an integration that started, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration: Option<Integration>,
    /// Journal describing cleanup that started, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<Cleanup>,
    /// When the result was sealed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at_unix: Option<u64>,
    /// When the owned worktree and branch were removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_at_unix: Option<u64>,
}

/// An immutable sealed result, persisted as `result.json`.
///
/// The patch is always computed against the pinned base rather than merely
/// against `HEAD`, so it captures committed, staged, unstaged, deleted, and
/// non-ignored untracked content as one complete change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleResult {
    /// Schema version of this record; always [`SCHEMA_VERSION`] when written.
    pub schema_version: u32,
    /// Capsule this result belongs to.
    pub capsule_id: String,
    /// Label carried over from the capsule at seal time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Opaque links carried over from the capsule at seal time.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    /// Shape of the result relative to its base.
    pub kind: ResultKind,
    /// Immutable commit the attempt started from.
    pub base_commit: String,
    /// Workspace `HEAD` at seal time.
    pub head_commit: String,
    /// SHA-256 of the sealed patch bytes.
    pub patch_sha256: String,
    /// Size of the sealed patch in bytes.
    pub patch_bytes: u64,
    /// Complete inventory of paths the result changes.
    pub changed_paths: Vec<String>,
    /// Total bytes of Git-ignored content present at seal time.
    ///
    /// Recorded as provenance about what the patch deliberately excludes.
    /// Ignored content may change afterwards without invalidating the seal.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ignored_bytes: u64,
    /// Structural digest of ignored content observed at seal time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_content_sha256: Option<String>,
    /// Ignored paths excluded from the patch, as reported by Git.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_paths: Vec<String>,
    /// Checkpoints made during the attempt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
    /// Evidence present when the result was sealed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    /// When the originating capsule was created.
    pub created_at_unix: u64,
    /// When the result was sealed.
    pub sealed_at_unix: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Observed condition of a capsule's workspace relative to its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleHealth {
    /// The workspace is present, owned, and consistent with its record.
    Healthy,
    /// The recorded workspace path no longer exists.
    MissingWorktree,
    /// Something else now occupies the workspace path.
    ForeignWorktree,
    /// Tracked content changed after the result was sealed.
    DriftedAfterClose,
    /// Creation was interrupted before the workspace became usable.
    IncompleteCreation,
    /// A checkpoint transition was interrupted.
    IncompleteCheckpoint,
    /// The workspace was intentionally removed by cleanup.
    Dropped,
}

/// A point-in-time inspection of one capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleStatus {
    /// The durable record as stored.
    pub capsule: Capsule,
    /// Condition of the workspace relative to that record.
    pub health: CapsuleHealth,
    /// Current workspace `HEAD`, when it can be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    /// Whether the workspace has uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// Non-ignored paths changed from the pinned base.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths: Vec<String>,
    /// Ignored untracked paths excluded from any result patch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_paths: Vec<String>,
    /// Commits reachable from `HEAD` but not from the base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_ahead: Option<u64>,
    /// After close, whether tracked content still matches the sealed result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed: Option<bool>,
}

/// Compact listing entry for a capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleSummary {
    /// Capsule identifier.
    pub id: String,
    /// Optional human-facing label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Current lifecycle position.
    pub state: CapsuleState,
    /// Ordinary filesystem directory the attempt works in.
    pub workspace_path: PathBuf,
    /// Immutable commit the attempt started from.
    pub base_commit: String,
    /// When the capsule was last mutated.
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

/// One interrupted transition that recovery was able to resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryAction {
    /// Capsule that was reconciled.
    pub capsule_id: String,
    /// State the capsule was found in.
    pub previous_state: CapsuleState,
    /// State it was moved to.
    pub state: CapsuleState,
    /// Human-readable description of what recovery did.
    pub action: String,
}

/// Which sealed artifact a descriptor refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// The sealed result manifest, `result.json`.
    ResultManifest,
    /// The complete binary-capable patch, `result.patch`.
    ResultPatch,
}

/// Location, size, and content address of one sealed artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    /// Which artifact this describes.
    pub kind: ArtifactKind,
    /// File name within a bundle, such as `result.patch`.
    pub name: String,
    /// IANA-style media type of the bytes.
    pub media_type: String,
    /// Percent-encoded `file://` URI of the artifact's current location.
    pub uri: String,
    /// Content address, formatted as `sha256:<digest>`.
    pub content_address: String,
    /// SHA-256 digest of the artifact bytes.
    pub sha256: String,
    /// Size of the artifact in bytes.
    pub bytes: u64,
}

/// The set of artifacts belonging to one sealed result.
///
/// Serialized as `bundle.json`, which is written last during an export and so
/// doubles as the completion marker for that directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactBundle {
    /// Schema version of this bundle; always [`BUNDLE_SCHEMA_VERSION`] when written.
    pub schema_version: u32,
    /// Capsule whose result these artifacts belong to.
    pub capsule_id: String,
    /// Descriptors for every artifact in the bundle.
    pub artifacts: Vec<ArtifactDescriptor>,
}

/// Outcome of exporting a sealed result to a new directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportReport {
    /// Bundle as written, with URIs pointing at the exported copies.
    pub bundle: ArtifactBundle,
    /// Directory the artifacts were written to.
    ///
    /// Canonicalized, so it may differ textually from the requested path when
    /// a parent directory is a symlink.
    pub output_directory: PathBuf,
}

/// Lifecycle transition an [`AuditEvent`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    /// A capsule and its workspace were created.
    Created,
    /// A checkpoint commit was completed.
    Checkpointed,
    /// Verification evidence was attached.
    EvidenceAdded,
    /// The result was sealed.
    Closed,
    /// Integration began and was journaled.
    IntegrationStarted,
    /// Integration failed before changing the target and was rolled back.
    IntegrationAborted,
    /// A sealed result was applied to a target worktree.
    Integrated,
    /// Cleanup began and was journaled.
    CleanupStarted,
    /// The owned worktree and branch were removed.
    Dropped,
    /// Recovery completed an interrupted transition.
    Recovered,
}

/// One structured record of a successful lifecycle transition.
///
/// Audit events are local administrative history. They are validated and
/// bounded, but they are neither signed nor append-only against someone who can
/// rewrite the state directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Schema version of this event; always [`AUDIT_SCHEMA_VERSION`] when written.
    pub schema_version: u32,
    /// Unique event identifier, formatted as `evt-<ulid>`.
    pub event_id: String,
    /// When the transition occurred.
    pub occurred_at_unix: u64,
    /// Which transition this records.
    pub kind: AuditEventKind,
    /// Capsule the event belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    /// Repository grouping key of that capsule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    /// State the capsule left, when the transition had a distinct origin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_state: Option<CapsuleState>,
    /// State the capsule entered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<CapsuleState>,
    /// Bounded transition details, such as a commit ID or patch digest.
    ///
    /// Evidence commands appear here by digest rather than verbatim.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

/// Summary of one stored record, readable even when its schema is unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRecordInspection {
    /// Directory name of the record.
    pub id: String,
    /// Declared schema version, when the manifest could be parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    /// Declared lifecycle state as raw text, without interpreting it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Whether a `result.json` is present alongside the manifest.
    pub has_result: bool,
    /// Why the record could not be summarized, when it could not be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Inventory of a state directory, independent of schema compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateInspection {
    /// Root directory that was inspected.
    pub state_root: PathBuf,
    /// Schema version this build can operate on.
    pub supported_schema_version: u32,
    /// Bytes of durable state, excluding workspaces and locks.
    pub state_bytes: u64,
    /// One entry per stored record, ordered by identifier.
    pub records: Vec<StateRecordInspection>,
}

/// Instantaneous aggregate counters across all capsules.
///
/// Computed on demand. There is no collector, exporter, or background job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// When the snapshot was taken.
    pub observed_at_unix: u64,
    /// Total durable capsule records.
    pub capsules: u64,
    /// Records not yet dropped.
    pub live_capsules: u64,
    /// Records carrying a sealed result.
    pub sealed_results: u64,
    /// Sum of sealed patch sizes in bytes.
    pub result_patch_bytes: u64,
    /// Bytes of durable state, excluding workspaces and locks.
    pub state_bytes: u64,
    /// Bytes occupied by live capsule workspaces.
    pub workspace_bytes: u64,
    /// Audit events currently retained.
    pub audit_events: u64,
    /// Audit events that rolled off retention.
    pub audit_events_dropped: u64,
    /// Capsule counts keyed by lifecycle state name.
    pub states: BTreeMap<String, u64>,
}

/// Outcome of copying durable state to a new directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupReport {
    /// State root that was copied.
    pub source: PathBuf,
    /// Directory the copy was written to.
    pub destination: PathBuf,
    /// Number of files copied.
    pub files: u64,
    /// Total bytes copied.
    pub bytes: u64,
}

/// Outcome of verifying an exported receipt.
///
/// Produced by [`verify_bundle`](crate::verify_bundle). Its presence means every
/// requested check passed; failures are reported as
/// [`Error::Verification`](crate::Error::Verification) instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    /// Directory that was verified.
    pub bundle_directory: PathBuf,
    /// Capsule the receipt belongs to.
    pub capsule_id: String,
    /// Shape of the sealed result.
    pub kind: ResultKind,
    /// Commit the result is pinned to.
    pub base_commit: String,
    /// Workspace `HEAD` recorded at seal time.
    pub head_commit: String,
    /// Size of the sealed patch in bytes.
    pub patch_bytes: u64,
    /// SHA-256 of the sealed patch.
    pub patch_sha256: String,
    /// Number of paths the result changes.
    pub changed_paths: usize,
    /// Evidence records present in the receipt.
    pub evidence_total: usize,
    /// Evidence records with a non-zero exit code.
    pub evidence_failed: usize,
    /// Whether the patch was additionally checked against a repository.
    pub repository_checked: bool,
}
