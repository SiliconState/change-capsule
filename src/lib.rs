pub mod artifact;
pub mod error;
mod git;
mod manager;
pub mod model;
pub mod policy;
mod state;

pub use artifact::{ArtifactReader, ArtifactSink, PublishedArtifact};
pub use error::{Error, Result};
pub use manager::{
    Author, CapsuleManager, CheckpointOptions, CloseOptions, CreateOptions, EvidenceInput,
    IntegrateOptions, MigrationOptions,
};
pub use model::{
    AUDIT_SCHEMA_VERSION, ArtifactBundle, ArtifactDescriptor, ArtifactKind, AuditEvent,
    AuditEventKind, BUNDLE_SCHEMA_VERSION, BackupReport, Capsule, CapsuleHealth, CapsuleResult,
    CapsuleState, CapsuleStatus, CapsuleSummary, Checkpoint, CheckpointJournal, Cleanup, Evidence,
    ExportReport, Integration, MetricsSnapshot, MigrationReport, RecoveryAction, ResultKind,
    ResultRef, SCHEMA_VERSION, StateInspection, StateRecordInspection,
};
pub use policy::{HARD_PATCH_BYTES, POLICY_SCHEMA_VERSION, Policy, PolicyReport};
pub use state::default_state_root;
