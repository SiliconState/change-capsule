//! Isolated, recoverable code-change attempts backed by ordinary Git worktrees,
//! sealed into portable receipts that can be verified anywhere.
//!
//! Capsule does not run an agent. It gives any agent or automation system
//! a safe place to work and produces a durable, inspectable result to hand back.
//!
//! # Model
//!
//! A *capsule* is one attempt at a change. It pins an exact base commit, owns an
//! ordinary Git worktree on its own branch, and records everything needed to
//! review the attempt later: checkpoints, caller-recorded verification evidence,
//! a complete binary-capable patch, a changed-path inventory, and content
//! digests. Several capsules can start from the same commit and modify the same
//! files without interfering, because each has its own worktree and index.
//!
//! Closing a capsule *seals* it. A sealed result is immutable: later mutation of
//! the workspace is detected as drift and blocks integration and ordinary
//! cleanup. Integration is always explicit and refuses anything but a clean
//! target still at the pinned base.
//!
//! # Receipts
//!
//! [`CapsuleManager::export_artifacts`] writes a sealed result to a
//! self-describing directory containing `bundle.json`, `result.json`, and
//! `result.patch`. That directory is a portable receipt: [`verify_bundle`]
//! re-checks it with no capsule state and no workspace, so the process that
//! produced a change and the process that reviews it never need to share a
//! machine. Given a repository, verification additionally proves the sealed
//! patch applies to the pinned base and reproduces exactly the sealed bytes and
//! changed paths.
//!
//! # Example
//!
//! ```no_run
//! use change_capsule::{CapsuleManager, CloseOptions, CreateOptions};
//!
//! let manager = CapsuleManager::open_default()?;
//!
//! let mut options = CreateOptions::new(".");
//! options.label = Some("candidate implementation".into());
//! let capsule = manager.create(options)?;
//!
//! // Launch any external tool with capsule.workspace_path as its directory,
//! // then record whatever verification the caller performed.
//!
//! let result = manager.close(&capsule.id, CloseOptions::default())?;
//! println!("sealed {} changed paths", result.changed_paths.len());
//! # Ok::<(), change_capsule::Error>(())
//! ```
//!
//! # Division of responsibility
//!
//! This crate owns the attempt lifecycle, provenance, artifacts, policy, audit
//! records, and state administration. The caller owns process launch, model
//! choice, prompts, credentials, sandboxing, verification execution, and any
//! remote artifact transport. The workspace is an isolation boundary for Git
//! state, not a security sandbox; run untrusted code under an external sandbox.
//!
//! # Feature flags
//!
//! The default `cli` feature builds the `capsule` binary and pulls in Clap.
//! Library-only embedders can disable default features.

pub mod artifact;
pub mod error;
mod git;
mod manager;
pub mod model;
pub mod policy;
mod state;
pub mod verify;

pub use artifact::{ArtifactReader, ArtifactSink, PublishedArtifact};
pub use error::{Error, Result};
pub use manager::{
    Author, CapsuleManager, CheckpointOptions, CloseOptions, CreateOptions, EvidenceInput,
    IntegrateOptions,
};
pub use model::{
    AUDIT_SCHEMA_VERSION, ArtifactBundle, ArtifactDescriptor, ArtifactKind, AuditEvent,
    AuditEventKind, BUNDLE_SCHEMA_VERSION, BackupReport, Capsule, CapsuleHealth, CapsuleResult,
    CapsuleState, CapsuleStatus, CapsuleSummary, Checkpoint, CheckpointJournal, Cleanup, Evidence,
    ExportReport, Integration, MetricsSnapshot, RecoveryAction, ResultKind, ResultRef,
    SCHEMA_VERSION, StateInspection, StateRecordInspection, VerificationReport,
};
pub use policy::{HARD_PATCH_BYTES, POLICY_SCHEMA_VERSION, Policy, PolicyReport};
pub use state::default_state_root;
pub use verify::{VerifyOptions, verify_bundle};
