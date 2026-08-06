use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const POLICY_SCHEMA_VERSION: u32 = 1;
pub const HARD_PATCH_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_repository_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_capsules: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_capsules: Option<u64>,
    #[serde(default = "default_patch_bytes")]
    pub max_patch_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_changed_paths: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ignored_paths: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ignored_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_capsule_age_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_state_bytes: Option<u64>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyReport {
    pub compliant: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<String>,
}

const fn default_patch_bytes() -> u64 {
    HARD_PATCH_BYTES
}
