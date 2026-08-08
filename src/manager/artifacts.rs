//! Sealed-artifact discovery, streaming, publication, and export.
//!
//! Part of the [`CapsuleManager`] implementation; see the parent module.

// This file is a continuation of the parent module's `impl CapsuleManager`,
// so it deliberately shares the parent's imports rather than duplicating a
// large, constantly churning list.
#[allow(clippy::wildcard_imports)]
use super::*;

impl CapsuleManager {
    /// Describe the sealed artifacts of a closed capsule.
    ///
    /// Revalidates the seal first, so descriptors always match live bytes.
    pub fn artifacts(&self, id: &str) -> Result<ArtifactBundle> {
        self.artifact_snapshot(id).map(|(bundle, _, _)| bundle)
    }

    /// Open one sealed artifact as a bounded reader over a validated snapshot.
    pub fn open_artifact(&self, id: &str, kind: ArtifactKind) -> Result<ArtifactReader> {
        let (_, result, patch) = self.artifact_snapshot(id)?;
        Ok(ArtifactReader::new(match kind {
            ArtifactKind::ResultManifest => result,
            ArtifactKind::ResultPatch => patch,
        }))
    }

    /// Stream every sealed artifact into a caller-provided sink.
    pub fn publish_artifacts<S: ArtifactSink + ?Sized>(
        &self,
        id: &str,
        sink: &mut S,
    ) -> Result<Vec<PublishedArtifact>> {
        let (bundle, result, patch) = self.artifact_snapshot(id)?;
        let mut result = Some(result);
        let mut patch = Some(patch);
        let mut published = Vec::with_capacity(bundle.artifacts.len());
        for descriptor in bundle.artifacts {
            let bytes = match descriptor.kind {
                ArtifactKind::ResultManifest => result.take(),
                ArtifactKind::ResultPatch => patch.take(),
            }
            .ok_or_else(|| {
                Error::UnsafeState(format!(
                    "artifact bundle contains duplicate kind {:?}",
                    descriptor.kind
                ))
            })?;
            let mut source = ArtifactReader::new(bytes);
            let uri = sink.put(&descriptor, &mut source)?;
            published.push(PublishedArtifact { descriptor, uri });
        }
        Ok(published)
    }

    /// Write a sealed result to a new directory as a portable receipt.
    ///
    /// Produces `result.json`, `result.patch`, and finally `bundle.json` as the
    /// completion marker. Verify the directory later with
    /// [`verify_bundle`](crate::verify_bundle).
    pub fn export_artifacts(
        &self,
        id: &str,
        destination: impl AsRef<Path>,
    ) -> Result<ExportReport> {
        let (bundle, result, patch) = self.artifact_snapshot(id)?;
        let destination = self.store.external_destination(destination.as_ref())?;
        let mut exported_bundle = bundle.clone();
        for descriptor in &mut exported_bundle.artifacts {
            // Reference artifacts relative to the bundle rather than by absolute
            // path. A receipt is meant to travel: an exporting machine's
            // directory layout is meaningless to whoever verifies it later, and
            // embedding it would publish that layout in any repository or
            // artifact store the receipt is committed to.
            descriptor.uri = descriptor.name.clone();
        }
        let mut manifest =
            serde_json::to_vec_pretty(&exported_bundle).map_err(|source| Error::Json {
                path: PathBuf::from("bundle.json"),
                source,
            })?;
        manifest.push(b'\n');
        StateStore::export_artifacts(
            &destination,
            &[
                ("bundle.json", &manifest),
                ("result.json", &result),
                ("result.patch", &patch),
            ],
        )?;
        Ok(ExportReport {
            bundle: exported_bundle,
            output_directory: destination,
        })
    }

    pub(super) fn artifact_snapshot(&self, id: &str) -> Result<(ArtifactBundle, Vec<u8>, Vec<u8>)> {
        let capsule = self.show(id)?;
        if capsule.result.is_none() {
            return Err(invalid_state(&capsule, "a sealed result"));
        }
        let (matches, _, result_bytes, patch) = self.sealed_artifact_snapshot(&capsule)?;
        if !matches {
            return Err(Error::ResultDrift(capsule.id));
        }
        let result_digest = sha256_hex(&result_bytes);
        let patch_digest = sha256_hex(&patch);
        let bundle = ArtifactBundle {
            schema_version: BUNDLE_SCHEMA_VERSION,
            capsule_id: id.to_owned(),
            artifacts: vec![
                artifact_descriptor(
                    ArtifactKind::ResultManifest,
                    "result.json",
                    "application/json",
                    &self.store.capsule_dir(id)?.join("result.json"),
                    &result_digest,
                    result_bytes.len() as u64,
                )?,
                artifact_descriptor(
                    ArtifactKind::ResultPatch,
                    "result.patch",
                    "application/vnd.git.patch",
                    &self.store.capsule_dir(id)?.join("result.patch"),
                    &patch_digest,
                    patch.len() as u64,
                )?,
            ],
        };
        Ok((bundle, result_bytes, patch))
    }

    pub(super) fn sealed_artifacts(
        &self,
        capsule: &Capsule,
    ) -> Result<(bool, CapsuleResult, Vec<u8>)> {
        let (matches, result, _, patch) = self.sealed_artifact_snapshot(capsule)?;
        Ok((matches, result, patch))
    }

    pub(super) fn sealed_artifact_snapshot(
        &self,
        capsule: &Capsule,
    ) -> Result<(bool, CapsuleResult, Vec<u8>, Vec<u8>)> {
        let Some(reference) = capsule.result.as_ref() else {
            return Err(invalid_state(capsule, "a sealed result"));
        };
        let (result, result_bytes) = self
            .store
            .read_result_artifact(&capsule.id)
            .map_err(|error| artifact_error(&capsule.id, error))?;
        let stored_patch = self
            .store
            .read_patch(&capsule.id)
            .map_err(|error| artifact_error(&capsule.id, error))?;
        let stored_digest = sha256_hex(&stored_patch);
        let matches = reference.kind == result.kind
            && reference.head_commit == result.head_commit
            && reference.patch_sha256 == stored_digest
            && reference.patch_sha256 == result.patch_sha256
            && reference.result_sha256 == result_sha256(&result)?
            && reference.patch_bytes == stored_patch.len() as u64
            && reference.patch_bytes == result.patch_bytes
            && reference.changed_paths == result.changed_paths.len()
            && reference.sealed_at_unix == result.sealed_at_unix
            && result.schema_version == SCHEMA_VERSION
            && result.capsule_id == capsule.id
            && result.label == capsule.label
            && result.links == capsule.links
            && result.base_commit == capsule.base_commit
            && result.checkpoints == capsule.checkpoints
            && result.evidence == capsule.evidence
            && result.created_at_unix == capsule.created_at_unix;
        Ok((matches, result, result_bytes, stored_patch))
    }
}
