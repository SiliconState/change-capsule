use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a Git repository: {0}")]
    NotRepository(PathBuf),
    #[error("capsule not found: {0}")]
    NotFound(String),
    #[error("invalid capsule id: {0}")]
    InvalidId(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("capsule {id} is {state}; expected {expected}")]
    InvalidState {
        id: String,
        state: String,
        expected: String,
    },
    #[error("unsafe state path: {0}")]
    UnsafeState(String),
    #[error("refusing to remove a foreign or replaced worktree: {0}")]
    ForeignWorktree(PathBuf),
    #[error("capsule has unsealed changes; close it first or pass --force: {0}")]
    UnsealedChanges(String),
    #[error("closed capsule has drifted since its result was sealed: {0}")]
    ResultDrift(String),
    #[error("integration target is not clean: {0}")]
    DirtyIntegrationTarget(PathBuf),
    #[error("Git command failed ({command}, exit {status}): {stderr}")]
    Git {
        command: String,
        status: i32,
        stderr: String,
    },
    #[error("Git output exceeded the {cap}-byte bound for: {command}")]
    GitOutputTooLarge { command: String, cap: usize },
    #[error("unsupported non-UTF-8 path: {0:?}")]
    NonUtf8Path(PathBuf),
    #[error("state schema version {found} is newer than supported version {supported}")]
    SchemaVersion { found: u32, supported: u32 },
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("internal worker failed while capturing Git output")]
    CaptureWorker,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
