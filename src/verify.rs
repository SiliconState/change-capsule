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
    ResultKind, VerificationReport,
};
use crate::policy::HARD_PATCH_BYTES;
use crate::state::{decode_versioned_json, read_bytes_bounded, validate_id};

const BUNDLE_JSON_CAP: u64 = 1024 * 1024;

/// What a verification run should require beyond structural consistency.
#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    /// Reject bundles whose evidence is absent or contains a non-zero exit code.
    pub require_successful_evidence: bool,
    /// When set, additionally confirm against this repository that the pinned
    /// base commit exists and that the sealed patch applies to it, reproducing
    /// exactly the sealed patch bytes and changed paths.
    pub repository: Option<PathBuf>,
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
/// let report = verify_bundle("./receipt", &VerifyOptions {
///     require_successful_evidence: true,
///     repository: Some(".".into()),
/// })?;
/// println!("{} changed {} path(s)", report.capsule_id, report.changed_paths);
/// # Ok::<(), change_capsule::Error>(())
/// ```
pub fn verify_bundle(
    directory: impl AsRef<Path>,
    options: &VerifyOptions,
) -> Result<VerificationReport> {
    let directory = directory.as_ref();
    let bundle_path = directory.join("bundle.json");
    let bundle_bytes = read_bytes_bounded(&bundle_path, BUNDLE_JSON_CAP)
        .map_err(|error| fail(format!("cannot read bundle.json: {error}")))?;
    let bundle: ArtifactBundle = serde_json::from_slice(&bundle_bytes)
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
        decode_versioned_json(&directory.join("result.json"), &result_bytes)
            .map_err(|error| fail(format!("result.json is not a valid sealed result: {error}")))?;
    verify_result_consistency(&bundle, &result, &patch)?;

    if options.require_successful_evidence
        && (result.evidence.is_empty() || result.evidence.iter().any(|item| item.exit_code != 0))
    {
        return Err(fail(
            "successful evidence is required, but evidence is absent or contains failures",
        ));
    }

    let repository_checked = if let Some(repository) = &options.repository {
        verify_against_repository(repository, &result, &patch)?;
        true
    } else {
        false
    };

    Ok(VerificationReport {
        bundle_directory: directory.to_path_buf(),
        capsule_id: result.capsule_id,
        kind: result.kind,
        base_commit: result.base_commit,
        head_commit: result.head_commit,
        patch_bytes: result.patch_bytes,
        patch_sha256: result.patch_sha256,
        changed_paths: result.changed_paths.len(),
        evidence_total: result.evidence.len(),
        evidence_failed: result
            .evidence
            .iter()
            .filter(|item| item.exit_code != 0)
            .count(),
        repository_checked,
    })
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

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn fail(message: impl Into<String>) -> Error {
    Error::Verification(message.into())
}
