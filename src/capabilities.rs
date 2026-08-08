//! Static machine-readable protocol capability negotiation.
//!
//! Capabilities describe externally meaningful compatibility contracts. They do
//! not inspect local state, invoke Git, or establish trust in the running binary.

use serde::Serialize;

use crate::model::{BUNDLE_SCHEMA_VERSION, SCHEMA_VERSION};

/// Schema version of the static capability document.
pub const CAPABILITY_SCHEMA_VERSION: u32 = 1;
/// Version of the orchestration protocol implemented by this build.
pub const PROTOCOL_VERSION: u32 = 1;
/// Schema version of local idempotency reservation records and lookup responses.
pub const IDEMPOTENCY_RECORD_SCHEMA_VERSION: u32 = 1;

/// Maximum capsule-label length in UTF-8 bytes.
pub const LABEL_BYTES_LIMIT: usize = 256;
/// Maximum number of opaque links on one capsule.
pub const LINKS_LIMIT: usize = 32;
/// Maximum link-key length in UTF-8 bytes.
pub const LINK_KEY_BYTES_LIMIT: usize = 64;
/// Maximum link-value length in UTF-8 bytes.
pub const LINK_VALUE_BYTES_LIMIT: usize = 4096;
/// Maximum idempotency-key length in UTF-8 bytes.
pub const IDEMPOTENCY_KEY_BYTES_LIMIT: usize = 256;

/// Schemas supported by the current build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CapabilitySchemas {
    /// Durable capsule/result schemas this build reads and writes.
    pub durable_read_write: Vec<u32>,
    /// Portable result schemas accepted by receipt verification.
    pub receipt_verify: Vec<u32>,
    /// Exported bundle schemas accepted by this build.
    pub bundle: Vec<u32>,
    /// Local idempotency reservation schemas accepted by this build.
    pub idempotency_record: Vec<u32>,
}

/// Stable input limits relevant to orchestration clients.
///
/// Every `*_bytes` field is a UTF-8 byte limit. `links` is a count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CapabilityLimits {
    /// Maximum label length in UTF-8 bytes.
    pub label_bytes: usize,
    /// Maximum number of opaque links.
    pub links: usize,
    /// Maximum link-key length in UTF-8 bytes.
    pub link_key_bytes: usize,
    /// Maximum link-value length in UTF-8 bytes.
    pub link_value_bytes: usize,
    /// Maximum idempotency-key length in UTF-8 bytes.
    pub idempotency_key_bytes: usize,
}

/// Static compatibility contract for this build.
///
/// Consumers should require a supported protocol version and a subset of
/// feature identifiers. Unknown additive fields and feature identifiers are
/// safe to ignore. This document does not authenticate the binary or its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Capabilities {
    /// Independent schema version of this capability document.
    pub capability_schema_version: u32,
    /// Stable product identifier.
    pub product: &'static str,
    /// Informational package version; not a substitute for feature negotiation.
    pub product_version: &'static str,
    /// Orchestration protocol versions supported by this build.
    pub protocol_versions: Vec<u32>,
    /// Stable versioned machine-contract identifiers.
    pub features: Vec<&'static str>,
    /// Durable and portable schemas supported by this build.
    pub schemas: CapabilitySchemas,
    /// Stable externally relevant limits.
    pub limits: CapabilityLimits,
}

impl Capabilities {
    /// Return the deterministic capability document for this build.
    pub fn current() -> Self {
        Self {
            capability_schema_version: CAPABILITY_SCHEMA_VERSION,
            product: "change-capsule",
            product_version: env!("CARGO_PKG_VERSION"),
            protocol_versions: vec![PROTOCOL_VERSION],
            features: vec![
                "cli.structured-errors.v1",
                "create.v1",
                "create.idempotent.v1",
                "idempotency.lookup.v1",
                "recover.targeted.v1",
                "diff.sha256.v1",
                "receipt.export.v1",
                "receipt.verify.v1",
                "receipt.attest.intoto.v1",
                "evidence.executed.v1",
            ],
            schemas: CapabilitySchemas {
                durable_read_write: vec![SCHEMA_VERSION],
                receipt_verify: vec![SCHEMA_VERSION],
                bundle: vec![BUNDLE_SCHEMA_VERSION],
                idempotency_record: vec![IDEMPOTENCY_RECORD_SCHEMA_VERSION],
            },
            limits: CapabilityLimits {
                label_bytes: LABEL_BYTES_LIMIT,
                links: LINKS_LIMIT,
                link_key_bytes: LINK_KEY_BYTES_LIMIT,
                link_value_bytes: LINK_VALUE_BYTES_LIMIT,
                idempotency_key_bytes: IDEMPOTENCY_KEY_BYTES_LIMIT,
            },
        }
    }
}
