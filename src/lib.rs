pub mod error;
mod git;
mod manager;
pub mod model;
mod state;

pub use error::{Error, Result};
pub use manager::{
    Author, CapsuleManager, CheckpointOptions, CloseOptions, CreateOptions, EvidenceInput,
    IntegrateOptions,
};
pub use model::{
    Capsule, CapsuleHealth, CapsuleResult, CapsuleState, CapsuleStatus, CapsuleSummary, Checkpoint,
    Evidence, Integration, RecoveryAction, ResultKind, ResultRef, SCHEMA_VERSION,
};
pub use state::default_state_root;
