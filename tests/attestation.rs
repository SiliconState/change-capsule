//! in-toto attestation conformance and trust-boundary tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use change_capsule::{
    CHANGE_PREDICATE_TYPE, CapsuleManager, CloseOptions, CreateOptions, EvidenceInput,
    IN_TOTO_STATEMENT_TYPE, VerifyOptions, attest_bundle,
};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    repo: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let state = temp.path().join("state");
        fs::create_dir(&repo).expect("create repo");
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "core.autocrlf", "false"]);
        fs::write(repo.join("tracked.txt"), b"base\n").expect("write base");
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-m",
                "base",
            ],
        );
        Self { temp, repo, state }
    }

    /// Seal a capsule with one successful, patch-bound evidence claim.
    fn sealed_receipt(&self) -> PathBuf {
        let manager = CapsuleManager::open(&self.state).expect("manager");
        let mut options = CreateOptions::new(&self.repo);
        options.label = Some("attested change".to_owned());
        options.links = BTreeMap::from([("task".to_owned(), "attest-1".to_owned())]);
        let capsule = manager.create(options).expect("create");
        fs::write(capsule.workspace_path.join("tracked.txt"), b"changed\n").expect("edit");
        manager
            .add_evidence(
                &capsule.id,
                EvidenceInput::claim("cargo test".to_owned(), 0)
                    .with_summary("all green".to_owned()),
            )
            .expect("evidence");
        manager
            .close(&capsule.id, CloseOptions::default())
            .expect("close");
        let receipt = self.temp.path().join("receipt");
        manager
            .export_artifacts(&capsule.id, &receipt)
            .expect("export");
        receipt
    }
}

fn plain() -> VerifyOptions {
    VerifyOptions::requiring(false, false, false)
}

/// in-toto discourages `.` and `$` in field names for query safety.
fn check_keys(value: &serde_json::Value) {
    if let Some(map) = value.as_object() {
        for (key, nested) in map {
            assert!(
                !key.contains('.') && !key.contains('$'),
                "field name {key} uses a discouraged character"
            );
            check_keys(nested);
        }
    } else if let Some(items) = value.as_array() {
        items.iter().for_each(check_keys);
    }
}

fn lowercase_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

#[test]
fn statement_conforms_to_in_toto_v1() {
    let fixture = Fixture::new();
    let receipt = fixture.sealed_receipt();
    let statement = attest_bundle(&receipt, &plain()).expect("attest");
    let value = serde_json::to_value(&statement).expect("statement JSON");

    // Exact type strings the spec mandates.
    assert_eq!(value["_type"], IN_TOTO_STATEMENT_TYPE);
    assert_eq!(value["_type"], "https://in-toto.io/Statement/v1");
    assert_eq!(value["predicateType"], CHANGE_PREDICATE_TYPE);
    assert!(value["predicate"].is_object());

    // "subject" is a non-empty array and every element MUST have a digest.
    let subject = value["subject"].as_array().expect("subject array");
    assert!(!subject.is_empty());
    for entry in subject {
        let digest = entry["digest"].as_object().expect("digest object");
        assert!(!digest.is_empty(), "each subject MUST set digest");
        for (algorithm, encoded) in digest {
            let encoded = encoded.as_str().expect("digest value is a string");
            assert!(
                lowercase_hex(encoded),
                "{algorithm} must be lowercase hex, got {encoded}"
            );
            match algorithm.as_str() {
                // Standard in-toto algorithm names, with the lengths the spec fixes.
                "sha256" => assert_eq!(encoded.len(), 64),
                "gitCommit" => assert!(matches!(encoded.len(), 40 | 64)),
                other => panic!("unexpected digest algorithm {other}"),
            }
        }
    }

    // Field names must avoid `.` and `$`, which the spec flags for query safety.
    check_keys(&value);
}

/// A statement with empty optional collections must survive a round trip.
///
/// `links` and `evidence` are omitted from the JSON when empty, so the
/// deserializer must treat their absence as empty rather than as an error.
#[test]
fn minimal_statement_round_trips_through_its_own_types() {
    let fixture = Fixture::new();
    let manager = CapsuleManager::open(&fixture.state).expect("manager");
    // No label, no links, no evidence: the sparsest sealable capsule.
    let capsule = manager
        .create(CreateOptions::new(&fixture.repo))
        .expect("create");
    fs::write(
        capsule.workspace_path.join("tracked.txt"),
        b"minimal
",
    )
    .expect("edit");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    let receipt = fixture.temp.path().join("minimal-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");

    let statement = attest_bundle(&receipt, &plain()).expect("attest");
    let bytes = serde_json::to_vec(&statement).expect("serialize");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("as value");
    assert!(value["predicate"].get("links").is_none(), "{value}");
    assert!(value["predicate"].get("evidence").is_none(), "{value}");
    let reparsed: change_capsule::Statement =
        serde_json::from_slice(&bytes).expect("omitted collections must deserialize as empty");
    assert_eq!(reparsed, statement);
}

#[test]
fn statement_is_deterministic_and_regenerable_from_the_receipt_alone() {
    let fixture = Fixture::new();
    let receipt = fixture.sealed_receipt();
    let first = serde_json::to_vec(&attest_bundle(&receipt, &plain()).expect("first")).unwrap();
    let second = serde_json::to_vec(&attest_bundle(&receipt, &plain()).expect("second")).unwrap();
    assert_eq!(
        first, second,
        "attestation must be a pure function of the receipt"
    );
}

#[test]
fn attestation_refuses_to_launder_a_tampered_receipt() {
    let fixture = Fixture::new();
    let receipt = fixture.sealed_receipt();
    assert!(attest_bundle(&receipt, &plain()).is_ok());

    let patch = receipt.join("result.patch");
    let mut bytes = fs::read(&patch).expect("patch");
    bytes.extend_from_slice(b"\n# injected\n");
    fs::write(&patch, &bytes).expect("tamper");

    assert!(
        attest_bundle(&receipt, &plain()).is_err(),
        "a receipt that fails verification must not yield a statement"
    );
}

#[test]
fn evidence_is_labelled_as_unexecuted_and_bound_only_when_it_really_is() {
    let fixture = Fixture::new();
    let receipt = fixture.sealed_receipt();
    let statement = attest_bundle(&receipt, &plain()).expect("attest");
    let value = serde_json::to_value(&statement).expect("json");
    let predicate = &value["predicate"];

    let claims = predicate["evidence"].as_array().expect("evidence");
    assert_eq!(claims.len(), 1);
    let claim = &claims[0];
    assert_eq!(claim["command"], "cargo test");
    assert_eq!(claim["exit_code"], 0);
    // Nothing ran this: the record must say so, and carry no output digest.
    assert_eq!(claim["executed"], false);
    assert!(claim.get("output_sha256").is_none());
    assert_eq!(claim["current_for_sealed_patch"], true);
    assert_eq!(
        claim["bound_patch_sha256"],
        value["subject"][0]["digest"]["sha256"]
    );

    // The proof boundary travels inside the document itself.
    let boundary = &predicate["proof_boundary"];
    let proves: Vec<&str> = boundary["proves"]
        .as_array()
        .expect("proves")
        .iter()
        .map(|v| v.as_str().expect("str"))
        .collect();
    let refutes: Vec<&str> = boundary["does_not_prove"]
        .as_array()
        .expect("does_not_prove")
        .iter()
        .map(|v| v.as_str().expect("str"))
        .collect();
    assert!(proves.contains(&"patch-applies-to-pinned-base"));
    assert!(refutes.contains(&"evidence-command-actually-ran"));
    assert!(refutes.contains(&"human-or-agent-authorship"));
}

#[test]
fn repository_bound_attestation_matches_the_verified_base() {
    let fixture = Fixture::new();
    let receipt = fixture.sealed_receipt();
    let options =
        VerifyOptions::requiring(false, false, false).with_repository(fixture.repo.clone());
    let statement = attest_bundle(&receipt, &options).expect("attest against repository");
    let head = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    assert_eq!(statement.predicate.base_commit, head.trim());
}

#[test]
fn cli_emits_statement_and_predicate_without_overwriting() {
    let fixture = Fixture::new();
    let receipt = fixture.sealed_receipt();

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_capsule"))
            .arg("attest")
            .arg(&receipt)
            .args(args)
            .output()
            .expect("attest CLI")
    };

    let full = run(&[]);
    assert!(
        full.status.success(),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    let statement: serde_json::Value = serde_json::from_slice(&full.stdout).expect("statement");
    assert_eq!(statement["_type"], IN_TOTO_STATEMENT_TYPE);

    // `--predicate-only` feeds `cosign attest-blob --predicate`.
    let predicate = run(&["--predicate-only"]);
    assert!(predicate.status.success());
    let body: serde_json::Value = serde_json::from_slice(&predicate.stdout).expect("predicate");
    assert!(body.get("_type").is_none());
    assert_eq!(body, statement["predicate"]);

    // `--output` publishes once and then refuses to clobber.
    let out = fixture.temp.path().join("statement.json");
    assert!(
        run(&["--output", out.to_str().expect("utf8")])
            .status
            .success()
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&out).expect("written")).unwrap(),
        statement
    );
    assert!(
        !run(&["--output", out.to_str().expect("utf8")])
            .status
            .success(),
        "attestation output must never overwrite"
    );
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_text(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run git");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("Git UTF-8")
}
