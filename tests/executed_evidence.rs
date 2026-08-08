//! Executed evidence.
//!
//! These cover the one guarantee that separates a receipt from a wish: that
//! Capsule ran the verification command itself, in the capsule workspace, and
//! recorded what it actually observed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use change_capsule::{
    CapsuleManager, CloseOptions, CreateOptions, Error, EvidenceInput, VerifyOptions, verify_bundle,
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
        fs::write(repo.join("shared.txt"), "base\n").expect("seed");
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
                "initial",
            ],
        );
        Self { temp, repo, state }
    }

    fn manager(&self) -> CapsuleManager {
        CapsuleManager::open(&self.state).expect("open manager")
    }

    fn create(&self, label: &str) -> change_capsule::Capsule {
        self.manager()
            .create(CreateOptions::new(&self.repo).with_label(label))
            .expect("create capsule")
    }
}

fn git(repo: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {arguments:?} failed");
}

/// A portable argument vector that exits with `code` and prints to both streams.
fn reporting_command(code: i32) -> Vec<String> {
    if cfg!(windows) {
        vec![
            "cmd".to_owned(),
            "/C".to_owned(),
            format!("echo out&echo err 1>&2&exit /b {code}"),
        ]
    } else {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("echo out; echo err 1>&2; exit {code}"),
        ]
    }
}

#[test]
fn executed_evidence_records_the_observed_exit_code_and_output_digest() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("executed");
    fs::write(capsule.workspace_path.join("shared.txt"), "worked\n").expect("edit");

    let passing = manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(0)))
        .expect("run passing command");
    assert!(passing.executed, "Capsule ran this itself");
    assert_eq!(passing.exit_code, 0);
    assert_eq!(
        passing.output_sha256.as_ref().map(String::len),
        Some(64),
        "executed evidence must digest what it captured"
    );
    assert!(passing.output_bytes.unwrap_or_default() > 0);
    assert!(
        passing.summary.unwrap_or_default().contains("err"),
        "the default summary should surface the tail of the output"
    );

    // The recorded exit code is observed, not supplied: a failing command is
    // recorded as failing even though the caller asked for nothing in particular.
    let failing = manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(3)))
        .expect("run failing command");
    assert_eq!(failing.exit_code, 3);
    assert!(failing.executed);
}

#[test]
fn executed_evidence_binds_to_the_patch_that_exists_after_the_run() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("binding");
    fs::write(capsule.workspace_path.join("shared.txt"), "worked\n").expect("edit");

    let evidence = manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(0)))
        .expect("run");
    let result = manager
        .close(&capsule.id, CloseOptions::executed())
        .expect("seal with executed evidence");
    assert_eq!(evidence.patch_sha256, result.patch_sha256);
}

#[test]
fn a_claim_can_never_satisfy_an_executed_evidence_requirement() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("claim-only");
    fs::write(capsule.workspace_path.join("shared.txt"), "asserted\n").expect("edit");

    // A caller can assert anything, including a passing run that never happened.
    let claim = manager
        .add_evidence(&capsule.id, EvidenceInput::claim("cargo test", 0))
        .expect("record claim");
    assert!(!claim.executed);
    assert!(claim.output_sha256.is_none());

    // The executed requirement rejects the assertion.
    let error = manager
        .close(&capsule.id, CloseOptions::executed())
        .expect_err("a claim must not satisfy an executed requirement");
    assert!(
        matches!(&error, Error::InvalidInput(message) if message.contains("executed evidence")),
        "{error}"
    );

    // The weaker requirement still accepts it, because it only asks that
    // something bound to this patch report success. That is the whole reason
    // the stronger level exists.
    manager
        .close(&capsule.id, CloseOptions::requiring(true, true, false))
        .expect("the weaker level accepts an assertion");
}

#[test]
fn a_failing_executed_command_cannot_seal_a_passing_receipt() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("failing");
    fs::write(capsule.workspace_path.join("shared.txt"), "broken\n").expect("edit");
    manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(1)))
        .expect("run failing command");
    let error = manager
        .close(&capsule.id, CloseOptions::executed())
        .expect_err("a failing run must not seal");
    assert!(
        matches!(&error, Error::InvalidInput(message) if message.contains("evidence")),
        "{error}"
    );
}

#[test]
fn a_timed_out_command_records_nothing() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("timeout");
    let sleeper = if cfg!(windows) {
        vec![
            "cmd".to_owned(),
            "/C".to_owned(),
            "ping -n 30 127.0.0.1 >NUL".to_owned(),
        ]
    } else {
        vec!["sleep".to_owned(), "30".to_owned()]
    };
    let error = manager
        .add_evidence(
            &capsule.id,
            EvidenceInput::run(sleeper).with_timeout(std::time::Duration::from_secs(1)),
        )
        .expect_err("the command must be killed");
    assert!(
        matches!(&error, Error::InvalidInput(message) if message.contains("timeout")),
        "{error}"
    );
    assert!(
        manager
            .show(&capsule.id)
            .expect("read capsule")
            .evidence
            .is_empty(),
        "a killed command must leave no evidence behind"
    );
}

/// Flipping `executed` on a claim must not buy a receipt past a strict gate.
///
/// Running a command is what produces an output digest and byte count, so a
/// record asserting `executed` without them cannot have come from one. Before
/// this was enforced, editing that single boolean turned a rejected receipt
/// into an accepted one.
#[test]
fn a_forged_executed_flag_cannot_pass_a_strict_gate() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("forgery");
    fs::write(capsule.workspace_path.join("shared.txt"), "forged\n").expect("edit");
    manager
        .add_evidence(&capsule.id, EvidenceInput::claim("cargo test", 0))
        .expect("claim");
    manager
        .close(&capsule.id, CloseOptions::requiring(true, true, false))
        .expect("seal claim");
    let receipt = fixture.temp.path().join("forged-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");

    // Honest starting point: the claim-only receipt is refused.
    verify_bundle(&receipt, &VerifyOptions::strict(&fixture.repo))
        .expect_err("a claim must not satisfy a strict gate");

    // Forge it: mark the claim executed, then repair the bundle descriptor so
    // the tampering is not caught by the ordinary digest check instead.
    let mut result: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.join("result.json")).expect("read result"))
            .expect("parse result");
    for record in result["evidence"].as_array_mut().expect("evidence array") {
        record["executed"] = serde_json::Value::Bool(true);
    }
    let forged = serde_json::to_vec(&result).expect("serialize forged result");
    fs::write(receipt.join("result.json"), &forged).expect("write forged result");
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.join("bundle.json")).expect("read bundle"))
            .expect("parse bundle");
    let digest = hex_digest(&forged);
    for artifact in bundle["artifacts"].as_array_mut().expect("artifacts") {
        if artifact["name"] == "result.json" {
            artifact["sha256"] = serde_json::Value::String(digest.clone());
            artifact["content_address"] = serde_json::Value::String(format!("sha256:{digest}"));
            artifact["bytes"] = serde_json::json!(forged.len());
        }
    }
    fs::write(
        receipt.join("bundle.json"),
        serde_json::to_vec(&bundle).expect("serialize bundle"),
    )
    .expect("write bundle");

    // The forgery is internally consistent by digest, and must still be refused.
    let error = verify_bundle(&receipt, &VerifyOptions::strict(&fixture.repo))
        .expect_err("a forged executed flag must not verify");
    assert!(
        matches!(&error, Error::Verification(message) if message.contains("captured-output digest")),
        "{error}"
    );
    // It must also fail plain integrity verification, not only the strict gate.
    verify_bundle(&receipt, &VerifyOptions::integrity())
        .expect_err("an incoherent record is not a valid receipt at any level");
}

fn hex_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// The ordinary agent loop must be able to seal: run, fail, fix, run, seal.
///
/// `require_executed_evidence` asks whether a passing executed record is bound
/// to the patch being sealed, which says nothing about earlier attempts. Folding
/// `require_successful_evidence` into the strict presets made the strongest mode
/// unusable for the workflow it exists to serve.
#[test]
fn an_attempt_that_failed_and_was_fixed_still_seals_and_verifies() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("iterated");

    fs::write(capsule.workspace_path.join("shared.txt"), "broken\n").expect("edit");
    let failed = manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(1)))
        .expect("first run fails");
    assert_eq!(failed.exit_code, 1);

    fs::write(capsule.workspace_path.join("shared.txt"), "fixed\n").expect("fix");
    manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(0)))
        .expect("second run passes");

    let result = manager
        .close(&capsule.id, CloseOptions::executed())
        .expect("a fixed attempt must still seal");
    assert_eq!(result.evidence.len(), 2, "history is kept, not rewritten");

    let receipt = fixture.temp.path().join("iterated-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    verify_bundle(&receipt, &VerifyOptions::strict(&fixture.repo))
        .expect("a gate must accept a receipt whose attempt failed once");

    // The stricter spotless-history option is still available, and still refuses.
    verify_bundle(
        &receipt,
        &VerifyOptions::requiring(true, false, false).with_repository(&fixture.repo),
    )
    .expect_err("require_successful_evidence really does mean every record");
}

/// Executed evidence bound to an older patch must not satisfy a later seal.
#[test]
fn stale_executed_evidence_cannot_seal_a_later_patch() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("stale");

    fs::write(capsule.workspace_path.join("shared.txt"), "tested\n").expect("edit");
    manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(0)))
        .expect("run against this patch");

    // Change the tree after the run. The passing record no longer describes what
    // would be sealed, and must not be accepted as though it did.
    fs::write(capsule.workspace_path.join("shared.txt"), "changed after\n").expect("late edit");
    let error = manager
        .close(&capsule.id, CloseOptions::executed())
        .expect_err("evidence bound to an older patch must not seal");
    assert!(
        matches!(&error, Error::InvalidInput(message) if message.contains("executed evidence")),
        "{error}"
    );

    // Re-running against the current tree restores the binding.
    manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(0)))
        .expect("re-run");
    manager
        .close(&capsule.id, CloseOptions::executed())
        .expect("seal once evidence describes the sealed patch");
}

/// A caller-supplied timeout must never be able to abort the process.
#[test]
fn an_unrepresentable_timeout_does_not_panic() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("huge-timeout");
    let evidence = manager
        .add_evidence(
            &capsule.id,
            EvidenceInput::run(reporting_command(0))
                .with_timeout(std::time::Duration::from_secs(u64::MAX)),
        )
        .expect("a deadline too far out to reach is simply no deadline");
    assert!(evidence.executed);
    assert_eq!(evidence.exit_code, 0);
}

/// An executed record must render its arguments unambiguously.
#[test]
fn recorded_command_keeps_argument_boundaries() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("quoting");
    let argv = if cfg!(windows) {
        vec!["cmd".to_owned(), "/C".to_owned(), "echo a b".to_owned()]
    } else {
        vec!["echo".to_owned(), "a b".to_owned(), "c".to_owned()]
    };
    let evidence = manager
        .add_evidence(&capsule.id, EvidenceInput::run(argv))
        .expect("run");
    assert!(
        evidence.command.contains("'a b'") || evidence.command.contains("'echo a b'"),
        "an argument containing a space must stay one argument: {}",
        evidence.command
    );
}

#[test]
fn receipts_carry_the_execution_distinction_to_an_offline_verifier() {
    let fixture = Fixture::new();
    let manager = fixture.manager();

    let claimed = fixture.create("claimed");
    fs::write(claimed.workspace_path.join("shared.txt"), "claimed\n").expect("edit");
    manager
        .add_evidence(&claimed.id, EvidenceInput::claim("cargo test", 0))
        .expect("claim");
    manager
        .close(&claimed.id, CloseOptions::requiring(true, true, false))
        .expect("seal claim");
    let claimed_receipt = fixture.temp.path().join("claimed-receipt");
    manager
        .export_artifacts(&claimed.id, &claimed_receipt)
        .expect("export");

    let executed = fixture.create("executed");
    fs::write(executed.workspace_path.join("shared.txt"), "executed\n").expect("edit");
    manager
        .add_evidence(&executed.id, EvidenceInput::run(reporting_command(0)))
        .expect("run");
    manager
        .close(&executed.id, CloseOptions::executed())
        .expect("seal executed");
    let executed_receipt = fixture.temp.path().join("executed-receipt");
    manager
        .export_artifacts(&executed.id, &executed_receipt)
        .expect("export");

    // Both receipts are internally valid, and both reproduce their tree.
    for receipt in [&claimed_receipt, &executed_receipt] {
        verify_bundle(
            receipt,
            &VerifyOptions::requiring(true, true, false).with_repository(&fixture.repo),
        )
        .expect("both receipts verify at the weaker level");
    }

    // Only one of them was actually verified by anything.
    let error = verify_bundle(&claimed_receipt, &VerifyOptions::strict(&fixture.repo))
        .expect_err("a claim-only receipt must fail a strict gate");
    assert!(
        matches!(&error, Error::Verification(message) if message.contains("executed evidence")),
        "{error}"
    );
    verify_bundle(&executed_receipt, &VerifyOptions::strict(&fixture.repo))
        .expect("an executed receipt passes a strict gate");
}

#[test]
fn attestation_widens_its_proof_boundary_only_for_executed_evidence() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("attested");
    fs::write(capsule.workspace_path.join("shared.txt"), "attested\n").expect("edit");
    manager
        .add_evidence(&capsule.id, EvidenceInput::run(reporting_command(0)))
        .expect("run");
    manager
        .close(&capsule.id, CloseOptions::executed())
        .expect("seal");
    let receipt = fixture.temp.path().join("attested-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");

    let statement = change_capsule::attest_bundle(&receipt, &VerifyOptions::strict(&fixture.repo))
        .expect("attest");
    let boundary = &statement.predicate.proof_boundary;
    assert!(
        boundary
            .proves
            .iter()
            .any(|claim| claim == "executed-evidence-ran-in-capsule-workspace")
    );
    assert!(
        !boundary
            .does_not_prove
            .iter()
            .any(|claim| claim == "evidence-command-actually-ran"),
        "an executed receipt must not still disclaim that the command ran"
    );
    assert!(
        boundary
            .does_not_prove
            .iter()
            .any(|claim| claim == "producing-host-was-uncompromised"),
        "execution never removes the need to trust the producing host"
    );
    assert!(
        statement
            .predicate
            .evidence
            .iter()
            .all(|item| item.executed)
    );
}

#[test]
fn cli_runs_a_command_and_refuses_to_mix_the_two_evidence_forms() {
    let fixture = Fixture::new();
    let capsule = fixture.create("cli-evidence");
    fs::write(capsule.workspace_path.join("shared.txt"), "cli\n").expect("edit");

    let mut arguments = vec![
        "--json".to_owned(),
        "evidence".to_owned(),
        capsule.id.clone(),
        "--".to_owned(),
    ];
    arguments.extend(reporting_command(0));
    let output = capsule_cli(&fixture, &arguments);
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("evidence json response");
    assert_eq!(value["executed"], serde_json::Value::Bool(true));
    assert_eq!(value["exit_code"], serde_json::json!(0));

    let rejected = capsule_cli(
        &fixture,
        &[
            "--json".to_owned(),
            "evidence".to_owned(),
            capsule.id.clone(),
            "--claim".to_owned(),
            "cargo test".to_owned(),
            "--exit-code".to_owned(),
            "0".to_owned(),
            "--".to_owned(),
            "true".to_owned(),
        ],
    );
    assert!(
        !rejected.status.success(),
        "passing both forms at once must be refused"
    );
}

fn capsule_cli(fixture: &Fixture, arguments: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(arguments)
        .env("CAPSULE_HOME", &fixture.state)
        .current_dir(&fixture.repo)
        .output()
        .expect("run capsule")
}
