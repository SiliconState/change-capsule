//! Error type shared by every operation in this crate.

use std::path::PathBuf;

/// Everything that can go wrong in a capsule operation.
///
/// Variants are deliberately specific so callers can branch on failure without
/// parsing messages. The `capsule` binary maps each variant to a stable `kind`
/// string in its JSON error output.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The given path is not inside a Git repository.
    #[error("not a Git repository: {0}")]
    NotRepository(PathBuf),
    /// No capsule with this identifier exists.
    #[error("capsule not found: {0}")]
    NotFound(String),
    /// The identifier is not a well-formed capsule ID.
    #[error("invalid capsule id: {0}")]
    InvalidId(String),
    /// Caller input was missing, malformed, or out of bounds.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// An existing key is permanently bound to a materially different creation request.
    #[error("idempotency key is already bound to a different creation request")]
    IdempotencyConflict,
    /// No reservation exists for the requested state-root-scoped idempotency key.
    #[error("idempotency reservation not found")]
    IdempotencyNotFound,
    /// The operation is not allowed from the capsule's current state.
    #[error("capsule {id} is {state}; expected {expected}")]
    InvalidState {
        /// Capsule the operation targeted.
        id: String,
        /// State it was actually in.
        state: String,
        /// State or states the operation requires.
        expected: String,
    },
    /// Stored state is inconsistent, unsafely shaped, or self-contradictory.
    #[error("unsafe state path: {0}")]
    UnsafeState(String),
    /// A path no longer proves it is the worktree the capsule created.
    ///
    /// Returned instead of deleting anything, even under `--force`.
    #[error("refusing to remove a foreign or replaced worktree: {0}")]
    ForeignWorktree(PathBuf),
    /// Cleanup was requested for a capsule whose work is not sealed.
    #[error("capsule has unsealed changes; close it first or pass --force: {0}")]
    UnsealedChanges(String),
    /// Tracked content or result artifacts changed after the result was sealed.
    #[error("closed capsule has drifted since its result was sealed: {0}")]
    ResultDrift(String),
    /// The integration target has uncommitted changes.
    #[error("integration target is not clean: {0}")]
    DirtyIntegrationTarget(PathBuf),
    /// A Git subprocess exited unsuccessfully.
    #[error("Git command failed ({command}, exit {status}): {stderr}")]
    Git {
        /// Command that was run.
        command: String,
        /// Exit status it returned.
        status: i32,
        /// Captured standard error, bounded in size.
        stderr: String,
    },
    /// A Git subprocess produced more output than its in-memory bound allows.
    #[error("Git output exceeded the {cap}-byte bound for: {command}")]
    GitOutputTooLarge {
        /// Command that produced the output.
        command: String,
        /// Bound that was exceeded, in bytes.
        cap: usize,
    },
    /// A path cannot be represented in the UTF-8 result inventory.
    #[error("unsupported non-UTF-8 path: {0:?}")]
    NonUtf8Path(PathBuf),
    /// Stored state was written by an incompatible schema version.
    #[error("state schema version {found} is incompatible with supported version {supported}")]
    SchemaVersion {
        /// Version found on disk.
        found: u32,
        /// Version this build supports.
        supported: u32,
    },
    /// The requested artifact is not part of this capsule's sealed result.
    #[error("artifact not found: {0}")]
    ArtifactNotFound(String),
    /// An exported receipt failed verification.
    #[error("bundle verification failed: {0}")]
    Verification(String),
    /// A filesystem operation failed.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path being operated on.
        path: PathBuf,
        /// Underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// Stored or supplied JSON could not be decoded or encoded.
    #[error("invalid JSON at {path}: {source}")]
    Json {
        /// Path the JSON came from.
        path: PathBuf,
        /// Underlying failure.
        #[source]
        source: serde_json::Error,
    },
    /// An internal thread draining Git output failed.
    #[error("internal worker failed while capturing Git output")]
    CaptureWorker,
}

/// Convenience alias for results produced by this crate.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
