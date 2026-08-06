//! Repository allowlists and resource limits enforced at lifecycle boundaries.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Schema version of `policy.json`, versioned independently of capsule state.
pub const POLICY_SCHEMA_VERSION: u32 = 1;

/// Absolute ceiling on sealed patch size, in bytes.
///
/// Policy may lower this but never raise it; it bounds in-memory patch
/// handling regardless of configuration.
pub const HARD_PATCH_BYTES: u64 = 64 * 1024 * 1024;

/// Repository and resource limits applied to capsule operations.
///
/// An absent `policy.json` means permissive defaults under
/// [`HARD_PATCH_BYTES`]. Every limit is optional, and usage that no configured
/// limit references is never measured, so the default policy adds no
/// filesystem walks or content inspection to lifecycle operations.
///
/// These are cooperative checkpoints evaluated at mutation boundaries, not
/// kernel-enforced quotas: a worker can grow a workspace between operations.
/// Use filesystem or OS quotas when continuous hard enforcement is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Schema version; must equal [`POLICY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Absolute roots capsules may be created from. Empty allows any repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_repository_roots: Vec<PathBuf>,
    /// Maximum durable capsule records, including dropped ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_capsules: Option<u64>,
    /// Maximum capsules not yet dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_capsules: Option<u64>,
    /// Maximum sealed patch size, capped by [`HARD_PATCH_BYTES`].
    ///
    /// Measured against the complete base-to-current result, including at a
    /// checkpoint boundary rather than only that checkpoint's delta.
    #[serde(default = "default_patch_bytes")]
    pub max_patch_bytes: u64,
    /// Maximum paths one result may change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_changed_paths: Option<u64>,
    /// Maximum Git-ignored paths permitted in a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ignored_paths: Option<u64>,
    /// Maximum bytes of Git-ignored content permitted in a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ignored_bytes: Option<u64>,
    /// Maximum age of a live capsule, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_capsule_age_seconds: Option<u64>,
    /// Maximum bytes of durable state, excluding workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_state_bytes: Option<u64>,
    /// Maximum bytes occupied by all live capsule workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workspace_bytes: Option<u64>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            schema_version: POLICY_SCHEMA_VERSION,
            allowed_repository_roots: Vec::new(),
            max_capsules: None,
            max_live_capsules: None,
            max_patch_bytes: HARD_PATCH_BYTES,
            max_changed_paths: None,
            max_ignored_paths: None,
            max_ignored_bytes: None,
            max_capsule_age_seconds: None,
            max_state_bytes: None,
            max_workspace_bytes: None,
        }
    }
}

impl Policy {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema_version != POLICY_SCHEMA_VERSION {
            return Err(Error::PolicyViolation(format!(
                "policy schema version {} is incompatible with supported version {POLICY_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if self.max_patch_bytes > HARD_PATCH_BYTES {
            return Err(Error::PolicyViolation(format!(
                "max_patch_bytes cannot exceed the hard safety bound of {HARD_PATCH_BYTES}"
            )));
        }
        if self.max_patch_bytes == 0 {
            return Err(Error::PolicyViolation(
                "max_patch_bytes must be greater than zero".to_owned(),
            ));
        }
        if self
            .allowed_repository_roots
            .iter()
            .any(|root| !root.is_absolute())
        {
            return Err(Error::PolicyViolation(
                "allowed repository roots must be absolute paths".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Result of evaluating current state against the effective policy.
///
/// Evaluation is observational and never mutates state. Usage that cannot be
/// inspected is reported as a violation rather than assumed compliant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyReport {
    /// Whether every evaluated limit was satisfied.
    pub compliant: bool,
    /// Human-readable description of each violation found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<String>,
}

const fn default_patch_bytes() -> u64 {
    HARD_PATCH_BYTES
}
