//! Offline verification of exported result bundles.
//!
//! A bundle produced by `capsule export` is a portable receipt: `bundle.json`,
//! `result.json`, and `result.patch`. Verification needs no capsule state
//! directory and no workspace, so any reviewer, CI job, or merge gate can
//! confirm that the diff it is looking at is exactly the diff that was sealed.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::git::Git;
use crate::model::{
    ArtifactBundle, ArtifactDescriptor, ArtifactKind, BUNDLE_SCHEMA_VERSION, CapsuleResult,
    LEGACY_SCHEMA_VERSION, ResultKind, SCHEMA_VERSION, VerificationReport,
};
use crate::policy::HARD_PATCH_BYTES;
use crate::signature::verify_bundle_signature_bytes;
use crate::state::{read_bytes_bounded, validate_id};

const BUNDLE_JSON_CAP: u64 = 1024 * 1024;

/// What a verification run should require beyond structural consistency.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct VerifyOptions {
    /// Reject bundles whose evidence is absent or contains a non-zero exit code.
    pub require_successful_evidence: bool,
    /// Reject unless successful evidence is bound to the sealed patch.
    pub require_current_successful_evidence: bool,
    /// When set, additionally confirm against this repository that the pinned
    /// base commit exists and that the sealed patch applies to it, reproducing
    /// exactly the sealed patch bytes and changed paths.
    pub repository: Option<PathBuf>,
}

impl VerifyOptions {
    /// Verification policy.
    ///
    /// Pass `repository` to additionally require that the sealed patch applies
    /// to its pinned base and reproduces exactly.
    pub fn new(
        require_successful_evidence: bool,
        require_current_successful_evidence: bool,
        repository: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            require_successful_evidence,
            require_current_successful_evidence,
            repository,
        }
    }
}

/// Verify an exported receipt directory.
///
/// Checks, in order: that `bundle.json` is a supported bundle naming exactly
/// the two expected artifacts; that `result.json` and `result.patch` match
/// their descriptors' digests and byte counts; and that the sealed result is
/// internally consistent, including that its patch digest, byte count, kind,
/// and changed-path list all agree with the patch bytes on disk.
///
/// With [`VerifyOptions::repository`] set, it additionally proves the pinned
/// base exists there and that applying the sealed patch to it reproduces
/// exactly the sealed bytes and changed paths. That step needs no clean
/// worktree and mutates nothing: the patch is applied to a private temporary
/// index.
///
/// Returns [`Error::Verification`] describing the first failed check, so the
/// message is safe to surface directly in CI output.
///
/// # Example
///
/// ```no_run
/// use change_capsule::{VerifyOptions, verify_bundle};
///
/// // Options types are `#[non_exhaustive]`, so build them with a constructor
/// // rather than a struct literal; new fields can then be added compatibly.
/// let report = verify_bundle(
///     "./receipt",
///     &VerifyOptions::new(true, false, Some(".".into())),
/// )?;
/// println!("{} changed {} path(s)", report.capsule_id, report.changed_paths);
/// # Ok::<(), change_capsule::Error>(())
/// ```
pub fn verify_bundle(
    directory: impl AsRef<Path>,
    options: &VerifyOptions,
) -> Result<VerificationReport> {
    let directory = directory.as_ref();
    let bundle_bytes = read_bundle_snapshot(directory)?;
    verify_bundle_snapshot(directory, &bundle_bytes, options)
}

/// Authenticate and verify one exact `bundle.json` byte snapshot.
///
/// The trusted public key is caller-supplied out of band. This function opens
/// `bundle.json` exactly once, authenticates those bytes, and uses those same
/// bytes for ordinary receipt verification. Receipt-contained key material is
/// never consulted.
pub fn verify_authenticated_bundle(
    directory: impl AsRef<Path>,
    signature: &[u8; 64],
    trusted_public_key: &[u8; 32],
    options: &VerifyOptions,
) -> Result<VerificationReport> {
    let directory = directory.as_ref();
    let bundle_bytes = read_bundle_snapshot(directory)?;
    verify_bundle_signature_bytes(&bundle_bytes, signature, trusted_public_key)?;
    let mut report = verify_bundle_snapshot(directory, &bundle_bytes, options)?;
    report.signature_authenticated = true;
    Ok(report)
}

fn read_bundle_snapshot(directory: &Path) -> Result<Vec<u8>> {
    read_bytes_bounded(&directory.join("bundle.json"), BUNDLE_JSON_CAP)
        .map_err(|error| fail(format!("cannot read bundle.json: {error}")))
}

fn verify_bundle_snapshot(
    directory: &Path,
    bundle_bytes: &[u8],
    options: &VerifyOptions,
) -> Result<VerificationReport> {
    verify_bundle_snapshot_full(directory, bundle_bytes, options).map(|(report, _)| report)
}

/// Verify a bundle and hand back the validated result alongside its report.
///
/// Attestation needs the decoded result, and must never be able to describe a
/// receipt that did not verify, so both come from this one checked path.
fn verify_bundle_snapshot_full(
    directory: &Path,
    bundle_bytes: &[u8],
    options: &VerifyOptions,
) -> Result<(VerificationReport, CapsuleResult)> {
    let bundle: ArtifactBundle = serde_json::from_slice(bundle_bytes)
        .map_err(|error| fail(format!("bundle.json is not a valid bundle: {error}")))?;
    if bundle.schema_version != BUNDLE_SCHEMA_VERSION {
        return Err(fail(format!(
            "bundle schema version {} is not the supported version {BUNDLE_SCHEMA_VERSION}",
            bundle.schema_version
        )));
    }
    validate_id(&bundle.capsule_id)
        .map_err(|_| fail(format!("invalid bundle capsule id: {}", bundle.capsule_id)))?;

    let manifest_descriptor = single_descriptor(&bundle, ArtifactKind::ResultManifest)?;
    let patch_descriptor = single_descriptor(&bundle, ArtifactKind::ResultPatch)?;
    if bundle.artifacts.len() != 2 {
        return Err(fail("bundle must describe exactly two artifacts"));
    }
    if manifest_descriptor.name != "result.json" || patch_descriptor.name != "result.patch" {
        return Err(fail(
            "bundle artifact names must be result.json and result.patch",
        ));
    }

    let result_bytes = read_bytes_bounded(&directory.join("result.json"), BUNDLE_JSON_CAP)
        .map_err(|error| fail(format!("cannot read result.json: {error}")))?;
    let patch = read_bytes_bounded(&directory.join("result.patch"), HARD_PATCH_BYTES)
        .map_err(|error| fail(format!("cannot read result.patch: {error}")))?;
    check_descriptor(manifest_descriptor, &result_bytes)?;
    check_descriptor(patch_descriptor, &patch)?;

    let result: CapsuleResult =
        decode_receipt_result(&directory.join("result.json"), &result_bytes)
            .map_err(|error| fail(format!("result.json is not a valid sealed result: {error}")))?;
    verify_result_consistency(&bundle, &result, &patch)?;

    if options.require_successful_evidence
        && (result.evidence.is_empty() || result.evidence.iter().any(|item| item.exit_code != 0))
    {
        return Err(fail(
            "successful evidence is required, but evidence is absent or contains failures",
        ));
    }

    if options.require_current_successful_evidence
        && !result.evidence.iter().any(|item| {
            item.exit_code == 0
                && item.patch_sha256.as_deref() == Some(result.patch_sha256.as_str())
        })
    {
        return Err(fail(
            "current successful evidence is required, but no successful evidence is bound to the sealed patch",
        ));
    }

    let repository_checked = if let Some(repository) = &options.repository {
        verify_against_repository(repository, &result, &patch)?;
        true
    } else {
        false
    };

    let report = VerificationReport {
        bundle_directory: directory.to_path_buf(),
        capsule_id: result.capsule_id.clone(),
        kind: result.kind,
        base_commit: result.base_commit.clone(),
        head_commit: result.head_commit.clone(),
        patch_bytes: result.patch_bytes,
        patch_sha256: result.patch_sha256.clone(),
        changed_paths: result.changed_paths.len(),
        evidence_total: result.evidence.len(),
        evidence_failed: result
            .evidence
            .iter()
            .filter(|item| item.exit_code != 0)
            .count(),
        repository_checked,
        signature_authenticated: false,
    };
    Ok((report, result))
}

/// Verify a receipt directory and return its validated sealed result.
pub(crate) fn verified_bundle_result(
    directory: &Path,
    options: &VerifyOptions,
) -> Result<(VerificationReport, CapsuleResult)> {
    let bundle_bytes = read_bundle_snapshot(directory)?;
    verify_bundle_snapshot_full(directory, &bundle_bytes, options)
}

fn single_descriptor(bundle: &ArtifactBundle, kind: ArtifactKind) -> Result<&ArtifactDescriptor> {
    let mut matches = bundle
        .artifacts
        .iter()
        .filter(|descriptor| descriptor.kind == kind);
    let descriptor = matches
        .next()
        .ok_or_else(|| fail(format!("bundle is missing a {kind:?} artifact")))?;
    if matches.next().is_some() {
        return Err(fail(format!("bundle has duplicate {kind:?} artifacts")));
    }
    Ok(descriptor)
}

fn check_descriptor(descriptor: &ArtifactDescriptor, bytes: &[u8]) -> Result<()> {
    let digest = hex::encode(Sha256::digest(bytes));
    if descriptor.bytes != bytes.len() as u64 {
        return Err(fail(format!(
            "{} is {} bytes but its descriptor records {}",
            descriptor.name,
            bytes.len(),
            descriptor.bytes
        )));
    }
    if descriptor.sha256 != digest {
        return Err(fail(format!(
            "{} does not match its descriptor digest",
            descriptor.name
        )));
    }
    if descriptor.content_address != format!("sha256:{digest}") {
        return Err(fail(format!(
            "{} content address does not match its bytes",
            descriptor.name
        )));
    }
    Ok(())
}

fn verify_result_consistency(
    bundle: &ArtifactBundle,
    result: &CapsuleResult,
    patch: &[u8],
) -> Result<()> {
    if !matches!(
        result.schema_version,
        SCHEMA_VERSION | LEGACY_SCHEMA_VERSION
    ) {
        return Err(fail("result schema version is unsupported"));
    }
    if result.capsule_id != bundle.capsule_id {
        return Err(fail(format!(
            "result capsule id {} does not match bundle capsule id {}",
            result.capsule_id, bundle.capsule_id
        )));
    }
    if !valid_object_id(&result.base_commit) || !valid_object_id(&result.head_commit) {
        return Err(fail("result base or head commit is malformed"));
    }
    if !valid_sha256(&result.patch_sha256)
        || result
            .ignored_content_sha256
            .as_ref()
            .is_some_and(|digest| !valid_sha256(digest))
    {
        return Err(fail("result digests are malformed"));
    }
    if result.patch_sha256 != hex::encode(Sha256::digest(patch)) {
        return Err(fail("result.patch does not match the sealed patch digest"));
    }
    if result.patch_bytes != patch.len() as u64 {
        return Err(fail("result.patch does not match the sealed byte count"));
    }
    if result.created_at_unix > result.sealed_at_unix {
        return Err(fail("result was sealed before it was created"));
    }
    if result.schema_version == SCHEMA_VERSION && result.ignored_content_sha256.is_none() {
        return Err(fail(
            "current result is missing its ignored-content structural digest",
        ));
    }
    if result
        .changed_paths
        .iter()
        .chain(&result.ignored_paths)
        .any(|path| !path.is_valid_encoding())
    {
        return Err(fail("result contains a non-canonical path encoding"));
    }
    if result.schema_version == LEGACY_SCHEMA_VERSION
        && result
            .evidence
            .iter()
            .any(|item| item.patch_sha256.is_some())
    {
        return Err(fail("legacy result contains impossible bound evidence"));
    }
    if result.schema_version == SCHEMA_VERSION
        && result.evidence.iter().any(|item| {
            item.patch_sha256
                .as_ref()
                .is_some_and(|digest| !valid_sha256(digest))
        })
    {
        return Err(fail("evidence patch digest is malformed"));
    }
    let empty = patch.is_empty();
    if (result.kind == ResultKind::NoChange) != empty {
        return Err(fail(
            "result kind does not agree with whether the sealed patch is empty",
        ));
    }
    if empty && !result.changed_paths.is_empty() {
        return Err(fail("empty result lists changed paths"));
    }
    if !empty && result.changed_paths.is_empty() {
        return Err(fail("non-empty result lists no changed paths"));
    }
    Ok(())
}

fn verify_against_repository(
    repository: &Path,
    result: &CapsuleResult,
    patch: &[u8],
) -> Result<()> {
    let git = Git::discover()?;
    let repository = git.repository(repository)?;
    let resolved = git
        .resolve_commit(&repository.worktree, &result.base_commit)
        .map_err(|error| {
            fail(format!(
                "pinned base {} is not available in the repository: {error}",
                result.base_commit
            ))
        })?;
    if resolved != result.base_commit {
        return Err(fail(format!(
            "pinned base resolved to a different commit: {resolved}"
        )));
    }
    if result.kind == ResultKind::NoChange {
        return Ok(());
    }
    let scratch = tempfile::tempdir()
        .map_err(|error| crate::error::io("temporary verification index", error))?;
    let index = scratch.path().join("index");
    let preview = git
        .apply_patch_preview(&repository.worktree, &result.base_commit, patch, &index)
        .map_err(|error| fail(format!("sealed patch does not apply to the base: {error}")))?;
    if preview.patch != patch {
        return Err(fail(
            "applying the sealed patch does not reproduce its exact bytes",
        ));
    }
    if preview.changed_paths != result.changed_paths {
        return Err(fail(
            "applying the sealed patch does not reproduce the sealed changed paths",
        ));
    }
    Ok(())
}

fn decode_receipt_result(path: &Path, bytes: &[u8]) -> Result<CapsuleResult> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| fail("result has no valid schema version"))?;
    if !matches!(version, SCHEMA_VERSION | LEGACY_SCHEMA_VERSION) {
        return Err(fail(format!("unsupported result schema version {version}")));
    }
    serde_json::from_value(value).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn fail(message: impl Into<String>) -> Error {
    Error::Verification(message.into())
}
