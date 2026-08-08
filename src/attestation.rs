//! in-toto attestation view of a sealed Capsule result.
//!
//! A Capsule receipt is self-contained and verifies offline with
//! [`crate::verify_bundle`]. This module additionally projects that same sealed
//! result into an [in-toto Statement v1], so a team already running in-toto,
//! SLSA, or Sigstore can consume Capsule output through the tooling it already
//! has instead of adopting a second format.
//!
//! The projection is lossless in the direction that matters: every field a
//! verifier needs is carried, and the statement is a pure function of the
//! sealed result, so anyone holding the receipt can regenerate byte-identical
//! output. Capsule never needs to be trusted to have produced it.
//!
//! # What this is not
//!
//! The statement is a *view*, not a second source of truth. The receipt remains
//! authoritative: `bundle.json` binds `result.json` and `result.patch` by digest,
//! and `capsule verify --repo` re-derives the tree from the pinned base. An
//! attestation that disagrees with the receipt it was generated from is simply
//! wrong, and the receipt wins.
//!
//! [in-toto Statement v1]: https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{CapsuleResult, GitPath, ResultKind};
use crate::verify::{VerifyOptions, verified_bundle_result};

/// `_type` of an in-toto Statement v1.
pub const IN_TOTO_STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";

/// `predicateType` identifying a Capsule change attestation.
///
/// Versioned independently of every other Capsule schema. A change to the
/// predicate's meaning requires a new URI, never a redefinition of this one.
pub const CHANGE_PREDICATE_TYPE: &str =
    "https://github.com/SiliconState/change-capsule/attestation/change/v1";

/// One in-toto `ResourceDescriptor`.
///
/// Only the fields Capsule populates are modelled. Consumers match subjects by
/// digest; `name` is a convenience label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResourceDescriptor {
    /// Human-facing label distinguishing this subject from the others.
    pub name: String,
    /// Digest set, keyed by in-toto algorithm name, lowercase hex encoded.
    pub digest: BTreeMap<String, String>,
}

/// What a Capsule receipt does and does not establish.
///
/// Carried inside the predicate on purpose. The single most common review
/// question about any change attestation is which claims are cryptographically
/// established and which are merely asserted by the producer; answering it in
/// the document itself means a consumer never has to go read prose to find out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProofBoundary {
    /// Claims a verifier can re-derive from the receipt plus the repository.
    pub proves: Vec<String>,
    /// Claims the receipt deliberately does not establish.
    pub does_not_prove: Vec<String>,
}

impl ProofBoundary {
    /// The boundary for a receipt, which depends on the evidence it carries.
    ///
    /// The first four entries hold for every receipt and need no trust at all:
    /// any holder recomputes them. Executed evidence adds one more, but that one
    /// is different in kind. It says the Capsule binary on the producing host ran
    /// the command and saw the result. A verifier must still trust that host, so
    /// `producing-host-was-uncompromised` stays on the other list either way.
    #[must_use]
    pub fn for_receipt(has_executed_evidence: bool) -> Self {
        let mut proves = vec![
            "patch-bytes-match-sealed-digest".to_owned(),
            "patch-applies-to-pinned-base".to_owned(),
            "patch-reproduces-sealed-bytes-and-changed-paths".to_owned(),
            "result-internally-consistent".to_owned(),
        ];
        let mut does_not_prove = vec![
            "human-or-agent-authorship".to_owned(),
            "code-quality-or-review-approval".to_owned(),
            "producing-host-was-uncompromised".to_owned(),
        ];
        if has_executed_evidence {
            proves.push("executed-evidence-ran-in-capsule-workspace".to_owned());
        } else {
            does_not_prove.push("evidence-command-actually-ran".to_owned());
            does_not_prove.push("evidence-output-is-truthful".to_owned());
        }
        Self {
            proves,
            does_not_prove,
        }
    }
}

/// One verification record, as carried in an attestation.
///
/// [`Self::executed`] is the field that matters. When it is true, Capsule ran
/// the command itself in the capsule workspace and observed the exit code and
/// the output digest. When it is false, the record is a caller assertion and
/// Capsule vouches for nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecordedEvidence {
    /// Exact command line.
    pub command: String,
    /// Exit status.
    pub exit_code: i32,
    /// Whether Capsule executed this command itself.
    pub executed: bool,
    /// Digest of the captured output. Present only for executed records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_sha256: Option<String>,
    /// Optional bounded summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Patch digest this record was bound to.
    pub bound_patch_sha256: String,
    /// Whether this record is bound to the exact patch in `subject`.
    pub current_for_sealed_patch: bool,
    /// When the record was attached.
    pub recorded_at_unix: u64,
}

/// Capsule change predicate, version 1.
///
/// Field values are encoded exactly as they appear in the sealed result, so a
/// consumer can compare them against `result.json` without normalisation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChangePredicate {
    /// Schema version of the sealed result this statement projects.
    pub result_schema_version: u32,
    /// Capsule the result belongs to.
    pub capsule_id: String,
    /// Shape of the result relative to its base.
    pub kind: ResultKind,
    /// Immutable commit the attempt started from.
    pub base_commit: String,
    /// Workspace `HEAD` at seal time.
    pub head_commit: String,
    /// Size of the sealed patch in bytes.
    pub patch_bytes: u64,
    /// Complete inventory of paths the result changes.
    pub changed_paths: Vec<GitPath>,
    /// Structural digest of Git-ignored content observed at seal time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored_content_sha256: Option<String>,
    /// Label carried over from the capsule at seal time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Opaque caller links carried over at seal time.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    /// Caller-asserted verification claims. Never executed by Capsule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<RecordedEvidence>,
    /// When the originating capsule was created.
    pub created_at_unix: u64,
    /// When the result was sealed.
    pub sealed_at_unix: u64,
    /// Machine-readable statement of this predicate's proof boundary.
    pub proof_boundary: ProofBoundary,
}

/// An in-toto Statement v1 carrying a [`ChangePredicate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Statement {
    /// Always [`IN_TOTO_STATEMENT_TYPE`].
    #[serde(rename = "_type")]
    pub statement_type: String,
    /// Artifacts this statement is about, matched by digest.
    pub subject: Vec<ResourceDescriptor>,
    /// Always [`CHANGE_PREDICATE_TYPE`].
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    /// The Capsule change predicate.
    pub predicate: ChangePredicate,
}

/// Verify a receipt directory, then project it into an in-toto Statement.
///
/// Verification is not optional here. An attestation is a claim other systems
/// act on, so Capsule refuses to produce one for a receipt it could not verify;
/// a broken or tampered bundle fails instead of being laundered into a
/// well-formed statement.
///
/// Pass `repository` in [`VerifyOptions`] to additionally require that the
/// sealed patch applies to its pinned base and reproduces exactly before
/// anything is emitted. The statement itself is identical either way: its
/// `proof_boundary` describes what any verifier *can* re-derive from the
/// receipt, not which checks this particular emission ran — deliberately, so
/// the statement stays a pure function of the receipt and a consumer verifies
/// by regenerating it rather than by trusting the emitter.
///
/// # Errors
///
/// Returns the underlying verification error if the receipt does not verify.
pub fn attest_bundle(directory: impl AsRef<Path>, options: &VerifyOptions) -> Result<Statement> {
    let (_report, result) = verified_bundle_result(directory.as_ref(), options)?;
    Ok(change_statement(&result))
}

/// Project a sealed result into an in-toto Statement.
///
/// Deterministic: the same result always yields byte-identical JSON, so any
/// holder of the receipt can regenerate and compare this document.
///
/// The subject carries the sealed patch by `sha256` and the result `HEAD` by
/// `gitCommit`, which are the two identities a merge gate binds.
pub fn change_statement(result: &CapsuleResult) -> Statement {
    let sealed = result.patch_sha256.as_str();
    let mut subject = vec![ResourceDescriptor {
        name: "result.patch".to_owned(),
        digest: BTreeMap::from([("sha256".to_owned(), result.patch_sha256.clone())]),
    }];
    // `gitCommit` is an in-toto standard algorithm whose value is the lowercase
    // hex object id, which is exactly how Capsule stores commits.
    subject.push(ResourceDescriptor {
        name: "head".to_owned(),
        digest: BTreeMap::from([("gitCommit".to_owned(), result.head_commit.clone())]),
    });

    let evidence: Vec<_> = result
        .evidence
        .iter()
        .map(|item| RecordedEvidence {
            command: item.command.clone(),
            exit_code: item.exit_code,
            executed: item.executed,
            output_sha256: item.output_sha256.clone(),
            summary: item.summary.clone(),
            current_for_sealed_patch: item.patch_sha256 == sealed,
            bound_patch_sha256: item.patch_sha256.clone(),
            recorded_at_unix: item.recorded_at_unix,
        })
        .collect();
    let has_executed_evidence = evidence
        .iter()
        .any(|item| item.executed && item.exit_code == 0 && item.current_for_sealed_patch);

    Statement {
        statement_type: IN_TOTO_STATEMENT_TYPE.to_owned(),
        subject,
        predicate_type: CHANGE_PREDICATE_TYPE.to_owned(),
        predicate: ChangePredicate {
            result_schema_version: result.schema_version,
            capsule_id: result.capsule_id.clone(),
            kind: result.kind,
            base_commit: result.base_commit.clone(),
            head_commit: result.head_commit.clone(),
            patch_bytes: result.patch_bytes,
            changed_paths: result.changed_paths.clone(),
            ignored_content_sha256: result.ignored_content_sha256.clone(),
            label: result.label.clone(),
            links: result.links.clone(),
            evidence,
            created_at_unix: result.created_at_unix,
            sealed_at_unix: result.sealed_at_unix,
            proof_boundary: ProofBoundary::for_receipt(has_executed_evidence),
        },
    }
}
