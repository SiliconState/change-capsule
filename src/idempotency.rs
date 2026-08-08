//! State-root-scoped idempotent capsule creation records and direct lookup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capabilities::{
    IDEMPOTENCY_KEY_BYTES_LIMIT, IDEMPOTENCY_RECORD_SCHEMA_VERSION, LABEL_BYTES_LIMIT,
    LINK_KEY_BYTES_LIMIT, LINK_VALUE_BYTES_LIMIT, LINKS_LIMIT,
};
use crate::error::{Error, Result};
use crate::model::Capsule;

pub(crate) const IDEMPOTENCY_RECORD_CAP: u64 = 256 * 1024;
pub(crate) const IDEMPOTENCY_KEY_CAP: usize = IDEMPOTENCY_KEY_BYTES_LIMIT;
const KEY_DOMAIN: &[u8] = b"change-capsule idempotency key v1\0";
const REQUEST_DOMAIN: &[u8] = b"change-capsule creation request v1\0";
const RECORD_DOMAIN: &[u8] = b"change-capsule idempotency reservation v1\0";

/// Whether an idempotency reservation has a durable capsule manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyStatus {
    /// The capsule identity is reserved but no manifest has been published.
    Reserved,
    /// A validated capsule manifest exists for the reserved identity.
    Materialized,
}

/// Direct lookup result for one state-root-scoped idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyLookup {
    /// Independent lookup/record schema version.
    pub schema_version: u32,
    /// Domain-separated SHA-256 of the caller-supplied key.
    pub idempotency_key_sha256: String,
    /// Capsule identity permanently reserved by the key.
    pub capsule_id: String,
    /// Whether the referenced capsule manifest exists.
    pub status: IdempotencyStatus,
    /// Validated current capsule manifest, once materialized.
    pub capsule: Option<Capsule>,
}

/// Administrative inspection of one indexed idempotency entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecordInspection {
    /// Indexed filename, never a raw idempotency key.
    pub filename: String,
    /// Declared record schema, when readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    /// Reserved capsule ID, when readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    /// Why the entry could not be validated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdempotencyRecord {
    pub(crate) schema_version: u32,
    pub(crate) idempotency_key_sha256: String,
    pub(crate) request_sha256: String,
    pub(crate) record_sha256: String,
    pub(crate) capsule_id: String,
    pub(crate) source_worktree: PathBuf,
    pub(crate) repository_common_dir: PathBuf,
    pub(crate) project_key: String,
    pub(crate) base_selector: String,
    pub(crate) base_commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) links: BTreeMap<String, String>,
    pub(crate) reserved_at_unix: u64,
}

impl IdempotencyRecord {
    /// Fill the derived digests of a reservation built from immutable inputs.
    ///
    /// Callers build the literal so every reserved field stays visible at the
    /// reservation site; only the two digests are derived here, and a record is
    /// never published or accepted with the placeholder values.
    pub(crate) fn sealed(mut self) -> Result<Self> {
        self.request_sha256 = self.canonical_request_sha256()?;
        self.record_sha256 = self.canonical_record_sha256()?;
        Ok(self)
    }

    pub(crate) fn validate(&self, expected_key_digest: &str) -> Result<()> {
        if self.schema_version != IDEMPOTENCY_RECORD_SCHEMA_VERSION
            || self.idempotency_key_sha256 != expected_key_digest
            || !valid_sha256(&self.idempotency_key_sha256)
            || !valid_sha256(&self.request_sha256)
            || !valid_sha256(&self.record_sha256)
            || !valid_capsule_id(&self.capsule_id)
            || !valid_project_key(&self.project_key)
            || !valid_object_id(&self.base_commit)
            || !self.source_worktree.is_absolute()
            || !self.repository_common_dir.is_absolute()
            || self.base_selector.is_empty()
            || self.base_selector.len() > 512
            || self.base_selector.starts_with('-')
            || self.base_selector.chars().any(char::is_control)
            || invalid_label(self.label.as_deref())
            || invalid_links(&self.links)
            || crate::state::project_key(&self.repository_common_dir)? != self.project_key
            || self.canonical_request_sha256()? != self.request_sha256
            || self.canonical_record_sha256()? != self.record_sha256
        {
            return Err(Error::UnsafeState(format!(
                "idempotency reservation {expected_key_digest} is malformed or contradictory"
            )));
        }
        Ok(())
    }

    pub(crate) fn canonical_record_sha256(&self) -> Result<String> {
        let mut digest = Sha256::new();
        digest.update(RECORD_DOMAIN);
        update_bytes(&mut digest, self.idempotency_key_sha256.as_bytes());
        update_bytes(&mut digest, self.request_sha256.as_bytes());
        update_bytes(&mut digest, self.capsule_id.as_bytes());
        update_path(&mut digest, &self.source_worktree)?;
        update_path(&mut digest, &self.repository_common_dir)?;
        update_bytes(&mut digest, self.project_key.as_bytes());
        update_bytes(&mut digest, self.base_selector.as_bytes());
        update_bytes(&mut digest, self.base_commit.as_bytes());
        digest.update(self.reserved_at_unix.to_be_bytes());
        Ok(hex::encode(digest.finalize()))
    }

    pub(crate) fn canonical_request_sha256(&self) -> Result<String> {
        canonical_request_sha256(
            &self.source_worktree,
            &self.repository_common_dir,
            &self.project_key,
            &self.base_selector,
            &self.base_commit,
            self.label.as_deref(),
            &self.links,
        )
    }
}

pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > IDEMPOTENCY_KEY_CAP
        || key
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(Error::InvalidInput(format!(
            "idempotency key must contain 1-{IDEMPOTENCY_KEY_CAP} UTF-8 bytes without whitespace or control characters"
        )));
    }
    Ok(())
}

pub(crate) fn key_sha256(key: &str) -> Result<String> {
    validate_key(key)?;
    let mut digest = Sha256::new();
    digest.update(KEY_DOMAIN);
    update_bytes(&mut digest, key.as_bytes());
    Ok(hex::encode(digest.finalize()))
}

pub(crate) fn canonical_request_sha256(
    source_worktree: &Path,
    repository_common_dir: &Path,
    project_key: &str,
    base_selector: &str,
    base_commit: &str,
    label: Option<&str>,
    links: &BTreeMap<String, String>,
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(REQUEST_DOMAIN);
    update_path(&mut digest, source_worktree)?;
    update_path(&mut digest, repository_common_dir)?;
    update_bytes(&mut digest, project_key.as_bytes());
    update_bytes(&mut digest, base_selector.as_bytes());
    update_bytes(&mut digest, base_commit.as_bytes());
    match label {
        Some(label) => {
            digest.update([1]);
            update_bytes(&mut digest, label.as_bytes());
        }
        None => digest.update([0]),
    }
    digest.update((links.len() as u64).to_be_bytes());
    for (key, value) in links {
        update_bytes(&mut digest, key.as_bytes());
        update_bytes(&mut digest, value.as_bytes());
    }
    Ok(hex::encode(digest.finalize()))
}

fn update_bytes(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

#[cfg_attr(any(unix, windows), allow(clippy::unnecessary_wraps))]
fn update_path(digest: &mut Sha256, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        digest.update(b"unix-bytes\0");
        update_bytes(digest, path.as_os_str().as_bytes());
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        digest.update(b"windows-utf16le\0");
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        digest.update((units.len() as u64).to_be_bytes());
        for unit in units {
            digest.update(unit.to_le_bytes());
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let value = path
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(path.to_path_buf()))?;
        digest.update(b"portable-utf8\0");
        update_bytes(digest, value.as_bytes());
        Ok(())
    }
}

fn invalid_label(label: Option<&str>) -> bool {
    label.is_some_and(|label| {
        label.trim().is_empty()
            || label.len() > LABEL_BYTES_LIMIT
            || label.chars().any(char::is_control)
    })
}

fn invalid_links(links: &BTreeMap<String, String>) -> bool {
    links.len() > LINKS_LIMIT
        || links.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > LINK_KEY_BYTES_LIMIT
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                || value.trim().is_empty()
                || value.len() > LINK_VALUE_BYTES_LIMIT
                || value.chars().any(char::is_control)
        })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_project_key(value: &str) -> bool {
    value.len() == 24
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_capsule_id(value: &str) -> bool {
    (8..=64).contains(&value.len())
        && value.starts_with("cap-")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}
