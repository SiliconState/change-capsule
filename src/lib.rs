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
//! # Orchestration protocol
//!
//! [`Capabilities::current`] is a static compatibility contract: an independent
//! capability schema version, protocol versions, stable versioned feature
//! identifiers, supported schemas, and byte limits. It touches no state and no
//! Git, so a coordinator can probe an unknown installation safely. It negotiates
//! protocol features only, never trust in the binary or its host.
//!
//! [`CapsuleManager::create_idempotent`] binds a caller-supplied opaque key to
//! one capsule identity within one canonical state root, publishing a durable
//! reservation before any capsule, branch, worktree, or manifest side effect, so
//! a retry after a timeout or crash resumes that same identity instead of
//! creating a second attempt. [`CapsuleManager::lookup_idempotency_key`] and
//! [`CapsuleManager::lookup_idempotency_key_at`] resolve one key directly,
//! without enumerating state. Keys are local orchestration metadata, not
//! credentials, and never appear in a portable receipt.
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
pub mod capabilities;
pub mod error;
mod git;
pub mod idempotency;
mod manager;
pub mod model;
mod path;
pub mod policy;
pub mod signature;
mod state;
pub mod verify;

pub use artifact::{ArtifactReader, ArtifactSink, PublishedArtifact};
pub use capabilities::{
    CAPABILITY_SCHEMA_VERSION, Capabilities, CapabilityLimits, CapabilitySchemas,
    IDEMPOTENCY_KEY_BYTES_LIMIT, IDEMPOTENCY_RECORD_SCHEMA_VERSION, LABEL_BYTES_LIMIT,
    LINK_KEY_BYTES_LIMIT, LINK_VALUE_BYTES_LIMIT, LINKS_LIMIT, PROTOCOL_VERSION,
};
pub use error::{Error, Result};
pub use idempotency::{IdempotencyLookup, IdempotencyRecordInspection, IdempotencyStatus};
pub use manager::{
    Author, CapsuleManager, CheckpointOptions, CloseOptions, CreateOptions, EvidenceInput,
    IntegrateOptions,
};
pub use model::{
    AUDIT_SCHEMA_VERSION, ArtifactBundle, ArtifactDescriptor, ArtifactKind, AuditEvent,
    AuditEventKind, BUNDLE_SCHEMA_VERSION, BackupReport, Capsule, CapsuleHealth, CapsuleResult,
    CapsuleState, CapsuleStatus, CapsuleSummary, Checkpoint, CheckpointJournal, Cleanup, Evidence,
    ExportReport, GitPath, Integration, LEGACY_SCHEMA_VERSION, MetricsSnapshot, MigrationReport,
    RecoveryAction, ResultKind, ResultRef, SCHEMA_VERSION, StateInspection, StateRecordInspection,
    VerificationReport,
};
pub use policy::{HARD_PATCH_BYTES, POLICY_SCHEMA_VERSION, Policy, PolicyReport};
pub use signature::{
    GeneratedKeypair, bundle_signature_commitment, derive_public_key, generate_keypair,
    sign_bundle, sign_bundle_bytes, verify_bundle_signature, verify_bundle_signature_bytes,
};
pub use state::default_state_root;
pub use verify::{VerifyOptions, verify_authenticated_bundle, verify_bundle};
