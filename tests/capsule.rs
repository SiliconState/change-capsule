//! End-to-end lifecycle, safety, recovery, and verification tests.
//!
//! Each test drives real Git repositories in temporary directories rather than
//! mocking the Git adapter, so failures reflect actual on-disk behaviour.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use fs2::FileExt;
use sha2::{Digest, Sha256};

use change_capsule::{
    ArtifactDescriptor, ArtifactKind, ArtifactSink, AuditEventKind, Author, CapsuleHealth,
    CapsuleManager, CapsuleState, CheckpointOptions, CloseOptions, CreateOptions, Error,
    EvidenceInput, GitPath, IntegrateOptions, Policy, ResultKind, VerifyOptions, sign_bundle,
    sign_bundle_bytes, verify_authenticated_bundle, verify_bundle, verify_bundle_signature_bytes,
};
use ed25519_dalek::SigningKey;
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
        git_success(&repo, ["init", "-b", "main"]);
        // Pin end-of-line handling so assertions compare capsule behaviour
        // rather than the host's Git EOL configuration. Linked worktrees and
        // integration targets share this repository config.
        git_success(&repo, ["config", "core.autocrlf", "false"]);
        git_success(&repo, ["config", "core.eol", "lf"]);
        fs::write(repo.join("shared.txt"), "base\n").expect("seed file");
        git_success(&repo, ["add", "."]);
        git_success(
            &repo,
            [
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
        let mut options = CreateOptions::new(&self.repo);
        options.label = Some(label.to_owned());
        options.links = BTreeMap::from([
            ("task".to_owned(), format!("task-{label}")),
            ("agent".to_owned(), "any-agent".to_owned()),
        ]);
        self.manager().create(options).expect("create capsule")
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn parallel_capsules_isolate_changes_and_integrate_one_explicit_result() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let mut first_options = CreateOptions::new(&fixture.repo);
    first_options.label = Some("first approach".to_owned());
    first_options.links = BTreeMap::from([("task".to_owned(), "task-42".to_owned())]);
    let first = manager.create(first_options).expect("first capsule");

    let mut second_options = CreateOptions::new(&fixture.repo);
    second_options.label = Some("second approach".to_owned());
    let second = manager.create(second_options).expect("second capsule");

    fs::write(first.workspace_path.join("shared.txt"), "first\n").expect("first edit");
    fs::write(second.workspace_path.join("shared.txt"), "second\n").expect("second edit");

    assert_eq!(
        fs::read_to_string(fixture.repo.join("shared.txt")).expect("read primary"),
        "base\n",
        "parallel attempts must not modify the primary worktree"
    );
    assert_eq!(
        manager
            .status(&first.id)
            .expect("first status")
            .changed_paths,
        vec!["shared.txt"]
    );
    assert_eq!(
        manager
            .status(&second.id)
            .expect("second status")
            .changed_paths,
        vec!["shared.txt"]
    );

    let checkpoint = manager
        .checkpoint(
            &first.id,
            CheckpointOptions::new("first implementation".to_owned(), test_author()),
        )
        .expect("checkpoint first");
    assert_eq!(checkpoint.commit.len(), 40);
    manager
        .add_evidence(
            &first.id,
            EvidenceInput::new("cargo test".to_owned(), 0)
                .with_summary("all tests passed".to_owned()),
        )
        .expect("first evidence");
    let first_result = manager
        .close(&first.id, CloseOptions::new(true, false))
        .expect("close first");
    let second_result = manager
        .close(&second.id, CloseOptions::default())
        .expect("close second");

    assert_eq!(first_result.kind, ResultKind::Commit);
    assert_eq!(second_result.kind, ResultKind::Patch);
    assert_ne!(first_result.patch_sha256, second_result.patch_sha256);
    assert_eq!(first_result.changed_paths, vec!["shared.txt"]);
    assert_eq!(second_result.changed_paths, vec!["shared.txt"]);

    drop(manager);
    let reopened = fixture.manager();
    assert_eq!(
        reopened.status(&first.id).expect("recovered first").health,
        CapsuleHealth::Healthy
    );
    assert_eq!(
        reopened.result(&second.id).expect("second result"),
        second_result
    );

    let integrated = reopened
        .integrate(
            &first.id,
            &IntegrateOptions::new(fixture.repo.clone(), test_author())
                .with_message("select first approach".to_owned()),
        )
        .expect("integrate first");
    assert_eq!(integrated.state, CapsuleState::Integrated);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("shared.txt")).expect("integrated file"),
        "first\n"
    );

    let second_error = reopened
        .integrate(
            &second.id,
            &IntegrateOptions::new(fixture.repo.clone(), test_author()),
        )
        .expect_err("stale second base must not integrate implicitly");
    assert!(
        matches!(second_error, Error::InvalidInput(message) if message.contains("does not equal pinned base"))
    );

    assert_eq!(
        reopened
            .drop_capsule(&first.id, false)
            .expect("drop first")
            .state,
        CapsuleState::Dropped
    );
    assert_eq!(
        reopened
            .drop_capsule(&second.id, false)
            .expect("drop second")
            .state,
        CapsuleState::Dropped
    );
    assert!(!first.workspace_path.exists());
    assert!(!second.workspace_path.exists());
    assert_eq!(
        fs::read_to_string(fixture.repo.join("shared.txt")).expect("primary survives cleanup"),
        "first\n"
    );
}

#[test]
fn sealed_result_detects_drift_and_force_cleanup_still_validates_ownership() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("drift");
    fs::write(capsule.workspace_path.join("shared.txt"), "sealed\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("seal capsule");

    fs::write(capsule.workspace_path.join("shared.txt"), "drifted\n").expect("drift capsule");
    let status = manager.status(&capsule.id).expect("drift status");
    assert_eq!(status.health, CapsuleHealth::DriftedAfterClose);
    assert_eq!(status.sealed, Some(false));
    assert!(matches!(
        manager.drop_capsule(&capsule.id, false),
        Err(Error::ResultDrift(id)) if id == capsule.id
    ));

    assert_eq!(
        manager
            .drop_capsule(&capsule.id, true)
            .expect("explicit force cleanup")
            .state,
        CapsuleState::Dropped
    );
}

#[test]
fn tampered_result_artifact_fails_seal_validation() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("artifact-tamper");
    fs::write(capsule.workspace_path.join("shared.txt"), "sealed\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("seal capsule");

    let patch = fixture
        .state
        .join("capsules")
        .join(&capsule.id)
        .join("result.patch");
    fs::write(&patch, "tampered\n").expect("tamper patch artifact");

    let status = manager.status(&capsule.id).expect("tampered status");
    assert_eq!(status.health, CapsuleHealth::DriftedAfterClose);
    assert_eq!(status.sealed, Some(false));
    assert!(matches!(
        manager.drop_capsule(&capsule.id, false),
        Err(Error::ResultDrift(id)) if id == capsule.id
    ));
    assert_eq!(
        manager
            .drop_capsule(&capsule.id, true)
            .expect("force cleanup after explicit review")
            .state,
        CapsuleState::Dropped
    );
}

#[test]
fn cleanup_refuses_a_replaced_foreign_directory_even_with_force() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("foreign");

    git_success(
        &fixture.repo,
        [
            "worktree",
            "remove",
            "--force",
            capsule.workspace_path.to_str().expect("utf8 workspace"),
        ],
    );
    fs::create_dir_all(&capsule.workspace_path).expect("replace workspace directory");
    git_success(&capsule.workspace_path, ["init", "-b", "foreign"]);
    fs::write(
        capsule.workspace_path.join("foreign.txt"),
        "do not delete\n",
    )
    .expect("foreign marker");

    assert!(matches!(
        manager.drop_capsule(&capsule.id, true),
        Err(Error::ForeignWorktree(path)) if path == capsule.workspace_path
    ));
    assert_eq!(
        fs::read_to_string(capsule.workspace_path.join("foreign.txt"))
            .expect("foreign directory survives"),
        "do not delete\n"
    );
}

#[test]
fn targeted_recovery_ignores_unrelated_malformed_records_and_is_idempotent() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let recoverable = fixture.create("targeted-recovery");
    let unrelated = fixture.create("malformed-unrelated");

    let manifest = manifest_path(&fixture, &recoverable.id);
    let mut value = read_json(&manifest);
    value["state"] = serde_json::Value::String("creating".to_owned());
    value["workspace_git_dir"] = serde_json::Value::Null;
    write_json(&manifest, &value);
    fs::write(manifest_path(&fixture, &unrelated.id), b"not json").expect("malform unrelated");

    let action = manager
        .recover_capsule(&recoverable.id)
        .expect("targeted recovery")
        .expect("recovery action");
    assert_eq!(action.capsule_id, recoverable.id);
    assert_eq!(action.state, CapsuleState::Active);
    assert!(
        manager
            .recover_capsule(&recoverable.id)
            .expect("idempotent targeted recovery")
            .is_none()
    );
    assert!(
        manager.recover().is_err(),
        "global recovery must fail closed"
    );
    assert_eq!(
        fs::read(manifest_path(&fixture, &unrelated.id)).expect("unrelated remains"),
        b"not json"
    );
}

#[test]
fn recover_finishes_a_journaled_creation_after_process_restart() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("recover");
    let original_manifest = read_json(&manifest_path(&fixture, &capsule.id));
    let workspace_git_dir = original_manifest["workspace_git_dir"].clone();
    let manifest = manifest_path(&fixture, &capsule.id);
    let mut value = read_json(&manifest);
    value["state"] = serde_json::Value::String("creating".to_owned());
    value["workspace_git_dir"] = serde_json::Value::Null;
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&value).expect("encode manifest"),
    )
    .expect("simulate interrupted journal");
    drop(manager);

    let reopened = fixture.manager();
    let actions = reopened.recover().expect("recover");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].capsule_id, capsule.id);
    assert_eq!(actions[0].previous_state, CapsuleState::Creating);
    assert_eq!(actions[0].state, CapsuleState::Active);
    assert_eq!(
        reopened
            .show(&capsule.id)
            .expect("recovered manifest")
            .workspace_git_dir,
        serde_json::from_value(workspace_git_dir).expect("workspace Git directory")
    );
}

#[cfg(unix)]
#[test]
fn state_and_manifests_are_owner_private() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let capsule = fixture.create("permissions");
    let state_mode = fs::metadata(&fixture.state)
        .expect("state metadata")
        .permissions()
        .mode()
        & 0o777;
    let manifest_mode = fs::metadata(
        fixture
            .state
            .join("capsules")
            .join(capsule.id)
            .join("capsule.json"),
    )
    .expect("manifest metadata")
    .permissions()
    .mode()
        & 0o777;
    assert_eq!(state_mode, 0o700);
    assert_eq!(manifest_mode, 0o600);
}

#[test]
fn recover_finalizes_a_checkpoint_commit_missing_its_manifest_update() {
    use sha2::{Digest, Sha256};

    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("checkpoint-recovery");
    fs::write(capsule.workspace_path.join("shared.txt"), "checkpointed\n").expect("edit capsule");
    let checkpoint = manager
        .checkpoint(
            &capsule.id,
            CheckpointOptions::new("recover this checkpoint".to_owned(), test_author()),
        )
        .expect("checkpoint");
    let head_before = git_text(
        &capsule.workspace_path,
        ["rev-parse", &format!("{}^", checkpoint.commit)],
    );
    let patch = git_bytes(
        &capsule.workspace_path,
        [
            "diff",
            "--binary",
            "--full-index",
            &head_before,
            &checkpoint.commit,
            "--",
        ],
    );
    let pending_ref = format!("refs/change-capsule/{}/checkpoint", capsule.id);
    git_success(
        &capsule.workspace_path,
        ["update-ref", &pending_ref, &checkpoint.commit],
    );
    git_success(&capsule.workspace_path, ["reset", "--mixed", &head_before]);
    let manifest = manifest_path(&fixture, &capsule.id);
    let mut value = read_json(&manifest);
    value["state"] = serde_json::Value::String("checkpointing".to_owned());
    value["checkpoints"] = serde_json::json!([]);
    value["checkpoint"] = serde_json::json!({
        "head_before": head_before,
        "head_after": checkpoint.commit,
        "patch_sha256": hex::encode(Sha256::digest(&patch)),
        "message": checkpoint.message,
        "author_name": checkpoint.author_name,
        "author_email": checkpoint.author_email,
        "started_at_unix": checkpoint.created_at_unix,
    });
    write_json(&manifest, &value);

    let actions = manager.recover().expect("recover checkpoint");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].previous_state, CapsuleState::Checkpointing);
    assert_eq!(actions[0].state, CapsuleState::Active);
    let recovered = manager.show(&capsule.id).expect("recovered capsule");
    assert_eq!(recovered.checkpoints, vec![checkpoint]);
    assert!(recovered.checkpoint.is_none());
    assert_eq!(
        git_text(&capsule.workspace_path, ["status", "--porcelain"]),
        ""
    );
}

#[test]
fn dropped_result_diff_json_uses_the_sealed_inventory() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("dropped-diff");
    fs::write(capsule.workspace_path.join("shared.txt"), "sealed\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    manager
        .drop_capsule(&capsule.id, false)
        .expect("drop capsule");

    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args([
            "--home",
            fixture.state.to_str().expect("utf8 state path"),
            "--json",
            "diff",
            &capsule.id,
        ])
        .output()
        .expect("run capsule CLI");
    assert_success(&output);
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse diff metadata");
    assert_eq!(value["changed_paths"], serde_json::json!(["shared.txt"]));
    assert!(value["bytes"].as_u64().is_some_and(|bytes| bytes > 0));
    let patch = manager.diff(&capsule.id).expect("sealed patch");
    assert_eq!(value["patch_sha256"], hex::encode(Sha256::digest(&patch)));
    assert!(value["patch_sha256"].as_str().is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }));
}

#[test]
fn json_diff_output_digest_covers_exact_written_patch() {
    let fixture = Fixture::new();
    let capsule = fixture.create("output-diff");
    fs::write(capsule.workspace_path.join("shared.txt"), "live\n").expect("edit capsule");
    let output_path = fixture.temp.path().join("live.patch");
    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args([
            "--home",
            fixture.state.to_str().expect("utf8 state path"),
            "--json",
            "diff",
            &capsule.id,
            "--output",
            output_path.to_str().expect("utf8 output path"),
        ])
        .output()
        .expect("run capsule CLI");
    assert_success(&output);
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse diff metadata");
    let written = fs::read(output_path).expect("read written patch");
    assert_eq!(value["bytes"], written.len());
    assert_eq!(value["patch_sha256"], hex::encode(Sha256::digest(&written)));
}

#[test]
fn json_mode_covers_cli_parse_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "show"])
        .output()
        .expect("run malformed capsule CLI");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("parse CLI error JSON");
    assert_eq!(value["ok"], false);
    assert_eq!(value["kind"], "cli");
}

#[test]
fn duplicate_link_keys_are_rejected_instead_of_silently_overwritten() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args([
            "--home",
            fixture.state.to_str().expect("utf8 state path"),
            "--json",
            "create",
            "--repo",
            fixture.repo.to_str().expect("utf8 repo path"),
            "--link",
            "task=one",
            "--link",
            "task=two",
        ])
        .output()
        .expect("run capsule CLI");
    assert!(!output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("parse duplicate-link error");
    assert_eq!(value["kind"], "invalid_input");
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|message| message.contains("duplicate link key"))
    );
}

#[test]
fn close_seals_ignored_content_inventory_as_provenance() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("ignored-provenance");
    fs::write(capsule.workspace_path.join(".gitignore"), "ignored.log\n")
        .expect("write ignore rule");
    fs::write(
        capsule.workspace_path.join("ignored.log"),
        "sealed ignored content\n",
    )
    .expect("write ignored content before close");
    let result = manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    assert_eq!(result.ignored_paths, vec!["ignored.log"]);
    assert!(result.ignored_bytes > 0);
    assert!(result.ignored_content_sha256.is_some());
}

#[test]
fn ignored_content_churn_after_close_does_not_block_integration_or_drop() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("ignored-churn");
    fs::write(capsule.workspace_path.join(".gitignore"), "build/\n").expect("write ignore rule");
    fs::write(capsule.workspace_path.join("shared.txt"), "sealed change\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    fs::create_dir(capsule.workspace_path.join("build")).expect("simulate build output");
    fs::write(
        capsule.workspace_path.join("build/artifact.bin"),
        "late build artifact\n",
    )
    .expect("write ignored content after close");

    let status = manager.status(&capsule.id).expect("status after churn");
    assert_eq!(status.health, CapsuleHealth::Healthy);
    assert_eq!(status.sealed, Some(true));
    assert_eq!(status.ignored_paths, vec!["build/"]);

    let integrated = manager
        .integrate(
            &capsule.id,
            &IntegrateOptions::new(fixture.repo.clone(), test_author())
                .with_message("integrate despite ignored churn"),
        )
        .expect("ignored churn must not block integration");
    assert_eq!(integrated.state, CapsuleState::Integrated);
    assert_eq!(
        manager
            .drop_capsule(&capsule.id, false)
            .expect("ignored churn must not block cleanup")
            .state,
        CapsuleState::Dropped
    );
}

#[test]
fn malformed_or_missing_result_artifacts_report_drift_in_status() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let first = fixture.create("malformed-result");
    fs::write(first.workspace_path.join("shared.txt"), "sealed\n").expect("edit capsule");
    manager
        .close(&first.id, CloseOptions::default())
        .expect("close capsule");
    fs::write(result_path(&fixture, &first.id), "not JSON\n").expect("corrupt result JSON");

    let status = manager
        .status(&first.id)
        .expect("malformed artifact status");
    assert_eq!(status.health, CapsuleHealth::DriftedAfterClose);
    assert_eq!(status.sealed, Some(false));

    let second = fixture.create("missing-patch");
    fs::write(second.workspace_path.join("shared.txt"), "sealed again\n").expect("edit capsule");
    manager
        .close(&second.id, CloseOptions::default())
        .expect("close second capsule");
    fs::remove_file(
        fixture
            .state
            .join("capsules")
            .join(&second.id)
            .join("result.patch"),
    )
    .expect("remove patch");
    let status = manager.status(&second.id).expect("missing artifact status");
    assert_eq!(status.health, CapsuleHealth::DriftedAfterClose);
    assert_eq!(status.sealed, Some(false));
}

#[test]
fn integration_into_a_detached_target_preserves_detached_head_identity() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("detached-target");
    fs::write(
        capsule.workspace_path.join("shared.txt"),
        "detached result\n",
    )
    .expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    let detached = fixture.temp.path().join("detached-target");
    git_success(
        &fixture.repo,
        [
            "worktree",
            "add",
            "--detach",
            detached.to_str().expect("utf8 detached path"),
            "HEAD",
        ],
    );
    let integrated = manager
        .integrate(
            &capsule.id,
            &IntegrateOptions::new(detached.clone(), test_author())
                .with_message("integrate detached".to_owned()),
        )
        .expect("integrate into detached target");
    assert_eq!(integrated.state, CapsuleState::Integrated);
    assert_eq!(
        integrated
            .integration
            .as_ref()
            .expect("integration journal")
            .target_head_ref,
        "HEAD"
    );
    assert_eq!(
        git_text(&detached, ["rev-parse", "--symbolic-full-name", "HEAD"]),
        "HEAD"
    );
    assert_eq!(
        fs::read_to_string(detached.join("shared.txt")).expect("read detached result"),
        "detached result\n"
    );
}

#[derive(Default)]
struct MemorySink {
    artifacts: Vec<(String, Vec<u8>)>,
}

struct MutatingSink {
    artifact_directory: PathBuf,
    artifacts: Vec<(String, Vec<u8>)>,
}

impl ArtifactSink for MutatingSink {
    fn put(
        &mut self,
        descriptor: &ArtifactDescriptor,
        source: &mut dyn Read,
    ) -> change_capsule::Result<String> {
        let path = self.artifact_directory.join(&descriptor.name);
        let mut tampered = fs::read(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let first = tampered.first_mut().expect("non-empty sealed artifact");
        *first ^= 1;
        fs::write(&path, tampered).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;

        let mut bytes = Vec::new();
        source
            .read_to_end(&mut bytes)
            .map_err(|source| Error::Io { path, source })?;
        self.artifacts.push((descriptor.name.clone(), bytes));
        Ok(format!("memory://{}", descriptor.content_address))
    }
}

impl ArtifactSink for MemorySink {
    fn put(
        &mut self,
        descriptor: &ArtifactDescriptor,
        source: &mut dyn Read,
    ) -> change_capsule::Result<String> {
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).map_err(|source| Error::Io {
            path: PathBuf::from(&descriptor.name),
            source,
        })?;
        self.artifacts.push((descriptor.name.clone(), bytes));
        Ok(format!("memory://{}", descriptor.content_address))
    }
}

#[test]
fn sealed_artifacts_support_discovery_streaming_publication_and_export() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("artifacts");
    fs::write(
        capsule.workspace_path.join("shared.txt"),
        "artifact result\n",
    )
    .expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    let bundle = manager.artifacts(&capsule.id).expect("discover artifacts");
    assert_eq!(bundle.capsule_id, capsule.id);
    assert_eq!(bundle.artifacts.len(), 2);
    assert!(bundle.artifacts.iter().all(|artifact| {
        artifact.uri.starts_with("file://")
            && artifact.content_address == format!("sha256:{}", artifact.sha256)
            && artifact.bytes > 0
    }));

    let mut patch = String::new();
    manager
        .open_artifact(&capsule.id, ArtifactKind::ResultPatch)
        .expect("open patch stream")
        .read_to_string(&mut patch)
        .expect("read patch stream");
    assert!(patch.contains("artifact result"));

    let mut sink = MemorySink::default();
    let published = manager
        .publish_artifacts(&capsule.id, &mut sink)
        .expect("publish artifacts");
    assert_eq!(published.len(), 2);
    assert_eq!(sink.artifacts.len(), 2);
    assert!(
        published
            .iter()
            .all(|artifact| artifact.uri.starts_with("memory://sha256:"))
    );

    let output = fixture.temp.path().join("exported");
    let report = manager
        .export_artifacts(&capsule.id, &output)
        .expect("export artifacts");
    // The reported directory is canonical, which differs from the requested
    // path on platforms where the temporary root is itself a symlink.
    assert_eq!(
        report
            .output_directory
            .canonicalize()
            .expect("canonical reported path"),
        output.canonicalize().expect("canonical requested path")
    );
    assert!(output.join("bundle.json").is_file());
    assert!(output.join("result.json").is_file());
    assert!(output.join("result.patch").is_file());
    assert!(
        report
            .bundle
            .artifacts
            .iter()
            .all(|artifact| artifact.content_address.starts_with("sha256:"))
    );
}

#[test]
fn artifact_publication_uses_one_validated_byte_snapshot() {
    use sha2::{Digest, Sha256};

    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("artifact-snapshot");
    fs::write(
        capsule.workspace_path.join("shared.txt"),
        "stable publication\n",
    )
    .expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    manager
        .artifacts(&capsule.id)
        .expect("validate artifacts before publication");
    let mut sink = MutatingSink {
        artifact_directory: fixture.state.join("capsules").join(&capsule.id),
        artifacts: Vec::new(),
    };
    let published = manager
        .publish_artifacts(&capsule.id, &mut sink)
        .expect("publish validated snapshot despite later file mutation");

    assert_eq!(published.len(), sink.artifacts.len());
    for (published, (name, bytes)) in published.iter().zip(&sink.artifacts) {
        assert_eq!(&published.descriptor.name, name);
        assert_eq!(published.descriptor.bytes, bytes.len() as u64);
        assert_eq!(
            published.descriptor.sha256,
            hex::encode(Sha256::digest(bytes))
        );
    }
    assert!(matches!(
        manager.result(&capsule.id),
        Err(Error::ResultDrift(id)) if id == capsule.id
    ));
}

#[test]
fn lifecycle_audit_events_and_metrics_are_runtime_neutral() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("audit");
    fs::write(capsule.workspace_path.join("shared.txt"), "audited\n").expect("edit capsule");
    manager
        .add_evidence(
            &capsule.id,
            EvidenceInput::new("cargo test".to_owned(), 0).with_summary("passed".to_owned()),
        )
        .expect("add evidence");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    let events = manager.audit_events(&capsule.id).expect("audit events");
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![
            AuditEventKind::Created,
            AuditEventKind::EvidenceAdded,
            AuditEventKind::Closed,
        ]
    );
    assert!(events.windows(2).all(|events| {
        events[0].occurred_at_unix <= events[1].occurred_at_unix
            && events[0].event_id != events[1].event_id
    }));
    let metrics = manager.metrics().expect("metrics");
    assert_eq!(metrics.capsules, 1);
    assert_eq!(metrics.live_capsules, 1);
    assert_eq!(metrics.sealed_results, 1);
    assert_eq!(metrics.audit_events, 3);
    assert_eq!(metrics.states.get("closed"), Some(&1));
}

#[test]
fn policy_enforces_repository_count_patch_and_ignored_limits() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    // Policy is #[non_exhaustive]: build from Default and set what matters.
    let mut policy = Policy::default();
    policy.allowed_repository_roots = vec![fixture.temp.path().to_path_buf()];
    policy.max_capsules = Some(1);
    policy.max_patch_bytes = 8;
    policy.max_ignored_paths = Some(0);
    manager.set_policy(policy).expect("set policy");

    let capsule = fixture.create("policy");
    assert!(matches!(
        manager.create(CreateOptions::new(&fixture.repo)),
        Err(Error::PolicyViolation(message)) if message.contains("capsule records")
    ));
    fs::write(
        capsule.workspace_path.join("shared.txt"),
        "larger than eight bytes\n",
    )
    .expect("edit capsule");
    assert!(matches!(
        manager.close(&capsule.id, CloseOptions::default()),
        Err(Error::PolicyViolation(message)) if message.contains("patch bytes")
    ));

    let mut policy = manager.policy().expect("read policy");
    policy.max_patch_bytes = change_capsule::HARD_PATCH_BYTES;
    manager.set_policy(policy).expect("relax patch policy");
    fs::write(capsule.workspace_path.join(".gitignore"), "ignored.log\n")
        .expect("write ignore rule");
    fs::write(capsule.workspace_path.join("ignored.log"), "ignored\n")
        .expect("write ignored content");
    assert!(matches!(
        manager.close(&capsule.id, CloseOptions::default()),
        Err(Error::PolicyViolation(message)) if message.contains("ignored paths")
    ));
    assert!(!manager.policy_report().expect("policy report").compliant);
}

#[test]
fn checkpoint_policy_applies_to_the_complete_capsule_result() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("checkpoint-policy");
    fs::write(capsule.workspace_path.join("first.txt"), "first\n").expect("first edit");
    manager
        .checkpoint(
            &capsule.id,
            CheckpointOptions::new("first checkpoint".to_owned(), test_author()),
        )
        .expect("first checkpoint");

    let mut policy = manager.policy().expect("read policy");
    policy.max_changed_paths = Some(1);
    manager.set_policy(policy).expect("set path policy");
    assert_eq!(
        manager
            .policy()
            .expect("read path policy")
            .max_changed_paths,
        Some(1)
    );
    let head_before = git_text(&capsule.workspace_path, ["rev-parse", "HEAD"]);
    fs::write(capsule.workspace_path.join("second.txt"), "second\n").expect("second edit");
    assert_eq!(
        manager.status(&capsule.id).expect("status").changed_paths,
        vec!["first.txt", "second.txt"]
    );

    let attempt = manager.checkpoint(
        &capsule.id,
        CheckpointOptions::new("second checkpoint".to_owned(), test_author()),
    );
    assert!(
        matches!(
            attempt,
            Err(Error::PolicyViolation(ref message)) if message.contains("changed paths")
        ),
        "unexpected checkpoint result: {attempt:?}"
    );
    assert_eq!(
        git_text(&capsule.workspace_path, ["rev-parse", "HEAD"]),
        head_before
    );
    assert_eq!(
        manager
            .show(&capsule.id)
            .expect("show capsule")
            .checkpoints
            .len(),
        1
    );
}

#[test]
fn policy_report_evaluates_active_results_and_reports_uninspectable_workspaces() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("active-policy-report");
    fs::write(
        capsule.workspace_path.join("shared.txt"),
        "active violation\n",
    )
    .expect("active edit");

    let mut policy = manager.policy().expect("read policy");
    policy.max_changed_paths = Some(0);
    manager.set_policy(policy).expect("set path policy");
    assert_eq!(
        manager
            .policy()
            .expect("read path policy")
            .max_changed_paths,
        Some(0)
    );
    assert_eq!(
        manager.status(&capsule.id).expect("status").changed_paths,
        vec!["shared.txt"]
    );
    let report = manager.policy_report().expect("report active violation");
    assert!(!report.compliant, "unexpected report: {report:?}");
    assert!(report.violations.iter().any(|violation| {
        violation.contains(&capsule.id) && violation.contains("changed paths")
    }));

    git_success(
        &fixture.repo,
        [
            "worktree",
            "remove",
            "--force",
            capsule.workspace_path.to_str().expect("utf8 workspace"),
        ],
    );
    let report = manager.policy_report().expect("report missing workspace");
    assert!(!report.compliant);
    assert!(report.violations.iter().any(|violation| {
        violation.contains(&capsule.id) && violation.contains("cannot be inspected")
    }));
}

#[test]
fn unsupported_schema_versions_fail_closed_but_remain_inspectable_and_backupable() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("old-schema");
    fs::write(capsule.workspace_path.join("shared.txt"), "old schema\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    let manifest_path = manifest_path(&fixture, &capsule.id);
    let mut manifest = read_json(&manifest_path);
    manifest["schema_version"] = serde_json::json!(2);
    write_json(&manifest_path, &manifest);

    assert!(matches!(
        manager.show(&capsule.id),
        Err(Error::SchemaVersion {
            found: 2,
            supported: 4
        })
    ));
    let inspection = manager.inspect_state().expect("inspect old state");
    assert_eq!(inspection.records[0].schema_version, Some(2));
    assert!(inspection.records[0].error.is_none());

    let backup = fixture.temp.path().join("old-schema-backup");
    let report = manager.backup_state(&backup).expect("backup old state");
    assert!(report.files > 0);
    assert!(
        backup
            .join("capsules")
            .join(&capsule.id)
            .join("capsule.json")
            .is_file()
    );
    assert!(backup.join("backup.json").is_file());
}

#[test]
fn result_seal_covers_provenance_and_rejects_metadata_tampering() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("provenance");
    fs::write(capsule.workspace_path.join("shared.txt"), "sealed\n").expect("edit capsule");
    let checkpoint = manager
        .checkpoint(
            &capsule.id,
            CheckpointOptions::new("preserve provenance".to_owned(), test_author()),
        )
        .expect("checkpoint");
    let result = manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    assert_eq!(result.label.as_deref(), Some("provenance"));
    assert_eq!(
        result.links.get("task"),
        Some(&"task-provenance".to_owned())
    );
    assert_eq!(result.checkpoints, vec![checkpoint]);
    assert_eq!(result.created_at_unix, capsule.created_at_unix);

    let path = result_path(&fixture, &capsule.id);
    let mut value = read_json(&path);
    value["changed_paths"] = serde_json::json!(["forged.txt"]);
    write_json(&path, &value);

    assert!(matches!(
        manager.result(&capsule.id),
        Err(Error::ResultDrift(id)) if id == capsule.id
    ));
}

#[test]
fn manifest_identity_fields_are_validated_before_use() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("identity");
    let path = manifest_path(&fixture, &capsule.id);
    let mut value = read_json(&path);
    value["branch"] = serde_json::Value::String("main".to_owned());
    write_json(&path, &value);

    assert!(matches!(
        manager.show(&capsule.id),
        Err(Error::UnsafeState(_))
    ));
    assert!(capsule.workspace_path.exists());
}

#[test]
fn recover_completes_cleanup_after_worktree_removal() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("cleanup-recovery");
    let branch_head = git_text(&capsule.workspace_path, ["rev-parse", "HEAD"]);
    let path = manifest_path(&fixture, &capsule.id);
    let mut value = read_json(&path);
    value["state"] = serde_json::Value::String("dropping".to_owned());
    value["cleanup"] = serde_json::json!({
        "branch_head": branch_head,
        "require_sealed": false,
        "started_at_unix": capsule.updated_at_unix,
    });
    write_json(&path, &value);
    git_success(
        &fixture.repo,
        [
            "worktree",
            "remove",
            "--force",
            capsule.workspace_path.to_str().expect("utf8 workspace"),
        ],
    );

    let actions = manager.recover().expect("recover cleanup");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].previous_state, CapsuleState::Dropping);
    assert_eq!(actions[0].state, CapsuleState::Dropped);
    assert_eq!(
        manager.show(&capsule.id).expect("dropped capsule").state,
        CapsuleState::Dropped
    );
    assert!(git_text(&fixture.repo, ["branch", "--list", &capsule.branch]).is_empty());
}

#[test]
fn integration_recovery_requires_the_exact_prepared_commit() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("exact-integration");
    fs::write(capsule.workspace_path.join("shared.txt"), "integrated\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    let integrated = manager
        .integrate(
            &capsule.id,
            &IntegrateOptions::new(fixture.repo.clone(), test_author())
                .with_message("expected integration".to_owned()),
        )
        .expect("integrate capsule");
    let expected_head = integrated
        .integration
        .as_ref()
        .and_then(|integration| integration.target_head_after.clone())
        .expect("expected integration head");
    let path = manifest_path(&fixture, &capsule.id);
    let mut value = read_json(&path);
    value["state"] = serde_json::Value::String("integrating".to_owned());
    value["integration"]["integrated_at_unix"] = serde_json::Value::Null;
    write_json(&path, &value);

    git_success(
        &fixture.repo,
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--amend",
            "-m",
            "unexpected replacement commit",
        ],
    );
    assert_ne!(
        git_text(&fixture.repo, ["rev-parse", "HEAD"]),
        expected_head
    );
    assert!(manager.recover().expect("conservative recovery").is_empty());
    assert_eq!(
        manager.show(&capsule.id).expect("journal remains").state,
        CapsuleState::Integrating
    );

    git_success(&fixture.repo, ["reset", "--hard", &expected_head]);
    let actions = manager.recover().expect("exact recovery");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].state, CapsuleState::Integrated);
}

#[cfg(unix)]
#[test]
fn configured_git_hooks_are_not_executed() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let hooks = fixture.temp.path().join("host-hooks");
    fs::create_dir(&hooks).expect("create hooks");
    let hook = hooks.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 99\n").expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).expect("make hook executable");
    git_success(
        &fixture.repo,
        [
            "config",
            "core.hooksPath",
            hooks.to_str().expect("utf8 hook path"),
        ],
    );

    let manager = fixture.manager();
    let capsule = fixture.create("hooks-disabled");
    fs::write(capsule.workspace_path.join("shared.txt"), "checkpoint\n").expect("edit capsule");
    manager
        .checkpoint(
            &capsule.id,
            CheckpointOptions::new("hook-free checkpoint".to_owned(), test_author()),
        )
        .expect("checkpoint must not run repository hook");
}

#[test]
fn option_like_base_revision_is_rejected_before_git_invocation() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let mut options = CreateOptions::new(&fixture.repo);
    options.base = "--help".to_owned();
    assert!(matches!(
        manager.create(options),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn capsule_created_from_sparse_source_has_a_complete_independent_checkout() {
    let fixture = Fixture::new();
    fs::write(fixture.repo.join("other.txt"), "other\n").expect("seed second tracked file");
    git_success(&fixture.repo, ["add", "other.txt"]);
    git_success(
        &fixture.repo,
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-m",
            "add second file",
        ],
    );
    git_success(
        &fixture.repo,
        ["sparse-checkout", "set", "--skip-checks", "shared.txt"],
    );
    assert_eq!(
        git_text(
            &fixture.repo,
            ["config", "--type=bool", "core.sparseCheckout"]
        ),
        "true"
    );

    let capsule = fixture.create("sparse-source");
    assert_eq!(
        fs::read_to_string(capsule.workspace_path.join("other.txt")).expect("complete checkout"),
        "other\n"
    );
    assert!(fixture.manager().status(&capsule.id).is_ok());
}

#[test]
fn skip_worktree_entries_do_not_hide_changes_from_the_temporary_index_snapshot() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("skip-worktree");
    git_success(
        &capsule.workspace_path,
        ["update-index", "--skip-worktree", "shared.txt"],
    );
    fs::write(
        capsule.workspace_path.join("shared.txt"),
        "changed despite skip\n",
    )
    .expect("edit hidden path");

    assert_eq!(
        manager
            .status(&capsule.id)
            .expect("complete snapshot")
            .changed_paths,
        vec!["shared.txt"]
    );
}

#[test]
fn assume_unchanged_entries_do_not_hide_changes_from_the_temporary_index_snapshot() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("assume-unchanged");
    git_success(
        &capsule.workspace_path,
        ["update-index", "--assume-unchanged", "shared.txt"],
    );
    fs::write(
        capsule.workspace_path.join("shared.txt"),
        "changed despite assume\n",
    )
    .expect("edit hidden path");

    assert_eq!(
        manager
            .status(&capsule.id)
            .expect("complete snapshot")
            .changed_paths,
        vec!["shared.txt"]
    );
}

#[test]
fn status_reports_ignored_untracked_content_excluded_from_results() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("ignored-content");
    fs::write(capsule.workspace_path.join(".gitignore"), "ignored.log\n")
        .expect("write ignore rule");
    fs::write(capsule.workspace_path.join("ignored.log"), "excluded\n")
        .expect("write ignored file");

    let status = manager
        .status(&capsule.id)
        .expect("status with ignored file");
    assert_eq!(status.changed_paths, vec![".gitignore"]);
    assert_eq!(status.ignored_paths, vec!["ignored.log"]);
    let result = manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close with ignored file");
    assert_eq!(result.changed_paths, vec![".gitignore"]);
}

#[test]
fn cli_exposes_artifacts_observability_policy_and_state_tools() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("cli-surfaces");
    fs::write(capsule.workspace_path.join("shared.txt"), "cli result\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    for (command, assertion) in [("artifacts", "artifacts"), ("audit", "created")] {
        let output = capsule_cli(&fixture, [command, &capsule.id]);
        assert_success(&output);
        let rendered = String::from_utf8(output.stdout).expect("utf8 CLI JSON");
        assert!(rendered.contains(assertion));
    }
    for command in [["metrics", ""], ["policy", "show"], ["state", "inspect"]] {
        let args: Vec<_> = command
            .into_iter()
            .filter(|argument| !argument.is_empty())
            .collect();
        let output = capsule_cli(&fixture, args);
        assert_success(&output);
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("parse CLI JSON");
    }

    let exported = fixture.temp.path().join("cli-export");
    let output = capsule_cli(
        &fixture,
        [
            "export",
            &capsule.id,
            "--output",
            exported.to_str().expect("utf8 export path"),
        ],
    );
    assert_success(&output);
    assert!(exported.join("bundle.json").is_file());

    let backup = fixture.temp.path().join("cli-backup");
    let output = capsule_cli(
        &fixture,
        [
            "state",
            "backup",
            "--output",
            backup.to_str().expect("utf8 backup path"),
        ],
    );
    assert_success(&output);
    assert!(backup.join("capsules").join(&capsule.id).is_dir());
}

#[cfg(unix)]
#[test]
fn export_and_backup_refuse_dangling_symlink_destinations() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("dangling-output");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");

    let export = fixture.temp.path().join("export-link");
    symlink("missing-export-target", &export).expect("create dangling export link");
    assert!(matches!(
        manager.export_artifacts(&capsule.id, &export),
        Err(Error::InvalidInput(message)) if message.contains("already exists")
    ));
    assert!(
        fs::symlink_metadata(&export)
            .expect("export link survives")
            .file_type()
            .is_symlink()
    );

    let backup = fixture.temp.path().join("backup-link");
    symlink("missing-backup-target", &backup).expect("create dangling backup link");
    assert!(matches!(
        manager.backup_state(&backup),
        Err(Error::InvalidInput(message)) if message.contains("already exists")
    ));
    assert!(
        fs::symlink_metadata(&backup)
            .expect("backup link survives")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn unregistered_embedded_repository_is_rejected_instead_of_becoming_a_gitlink() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("embedded-repository");
    let embedded = capsule.workspace_path.join("vendor/embedded");
    fs::create_dir_all(&embedded).expect("create embedded repository");
    git_success(&embedded, ["init", "-b", "main"]);
    fs::write(embedded.join("nested.txt"), "nested\n").expect("seed embedded repository");
    git_success(&embedded, ["add", "."]);
    git_success(
        &embedded,
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-m",
            "embedded base",
        ],
    );

    assert!(matches!(
        manager.status(&capsule.id),
        Err(Error::InvalidInput(message)) if message.contains("unregistered embedded Git repository")
    ));
}

#[test]
fn dirty_submodule_content_is_rejected_instead_of_silently_omitted() {
    let fixture = Fixture::new();
    let submodule = fixture.temp.path().join("submodule");
    fs::create_dir(&submodule).expect("create submodule repository");
    git_success(&submodule, ["init", "-b", "main"]);
    fs::write(submodule.join("nested.txt"), "base\n").expect("seed submodule");
    git_success(&submodule, ["add", "."]);
    git_success(
        &submodule,
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-m",
            "submodule base",
        ],
    );
    git_success(
        &fixture.repo,
        [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            submodule.to_str().expect("utf8 submodule"),
            "deps/submodule",
        ],
    );
    git_success(&fixture.repo, ["add", "."]);
    git_success(
        &fixture.repo,
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-m",
            "add submodule",
        ],
    );

    let manager = fixture.manager();
    let capsule = fixture.create("dirty-submodule");
    git_success(
        &capsule.workspace_path,
        [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    );
    fs::write(
        capsule.workspace_path.join("deps/submodule/nested.txt"),
        "dirty\n",
    )
    .expect("dirty submodule");

    assert!(matches!(
        manager.status(&capsule.id),
        Err(Error::InvalidInput(message)) if message.contains("dirty submodule")
    ));
    assert!(matches!(
        manager.close(&capsule.id, CloseOptions::default()),
        Err(Error::InvalidInput(message)) if message.contains("dirty submodule")
    ));
}

#[test]
fn exported_receipts_carry_no_trace_of_the_exporting_machine() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("portable-receipt");
    fs::write(capsule.workspace_path.join("shared.txt"), "portable\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    let exported = fixture.temp.path().join("portable-bundle");
    manager
        .export_artifacts(&capsule.id, &exported)
        .expect("export bundle");

    // A receipt is committed to repositories and shipped to third parties, so
    // it must not disclose the exporting machine's directory layout.
    let host_markers = [
        exported.to_str().expect("utf8 export path"),
        fixture.temp.path().to_str().expect("utf8 temp path"),
        "file://",
        "/home/",
        "/Users/",
        "C:\\",
    ];
    for artifact in ["bundle.json", "result.json", "result.patch"] {
        let body = fs::read_to_string(exported.join(artifact))
            .unwrap_or_else(|_| String::from("<binary>"));
        for marker in host_markers {
            assert!(
                !body.contains(marker),
                "{artifact} leaks host path marker {marker:?}"
            );
        }
    }

    let bundle: serde_json::Value =
        serde_json::from_slice(&fs::read(exported.join("bundle.json")).expect("read bundle"))
            .expect("parse bundle");
    for artifact in bundle["artifacts"].as_array().expect("artifacts") {
        assert_eq!(
            artifact["uri"], artifact["name"],
            "exported artifacts must be referenced relative to the bundle"
        );
    }

    // Relocating the bundle must not affect verification.
    let moved = fixture.temp.path().join("relocated-bundle");
    fs::rename(&exported, &moved).expect("relocate bundle");
    let report = verify_bundle(&moved, &VerifyOptions::default()).expect("verify relocated bundle");
    assert_eq!(report.capsule_id, capsule.id);
}

#[test]
fn exported_bundles_verify_offline_and_detect_tampering() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("verify");
    fs::write(capsule.workspace_path.join("shared.txt"), "verified\n").expect("edit capsule");
    manager
        .add_evidence(
            &capsule.id,
            EvidenceInput::new("cargo test".to_owned(), 0).with_summary("passed".to_owned()),
        )
        .expect("add evidence");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    let exported = fixture.temp.path().join("verify-bundle");
    manager
        .export_artifacts(&capsule.id, &exported)
        .expect("export bundle");

    let report = verify_bundle(
        &exported,
        &VerifyOptions::new(true, false, Some(fixture.repo.clone())),
    )
    .expect("verify exported bundle");
    assert_eq!(report.capsule_id, capsule.id);
    assert_eq!(report.kind, ResultKind::Patch);
    assert_eq!(report.changed_paths, 1);
    assert_eq!(report.evidence_total, 1);
    assert_eq!(report.evidence_failed, 0);
    assert!(report.repository_checked);

    let mut tampered = fs::read(exported.join("result.patch")).expect("read exported patch");
    let first = tampered.first_mut().expect("non-empty patch");
    *first ^= 1;
    fs::write(exported.join("result.patch"), tampered).expect("tamper exported patch");
    assert!(matches!(
        verify_bundle(&exported, &VerifyOptions::default()),
        Err(Error::Verification(message)) if message.contains("result.patch")
    ));
}

#[test]
fn verification_confirms_the_patch_reproduces_exactly_against_the_base() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("verify-repo");
    fs::write(capsule.workspace_path.join("shared.txt"), "reproduced\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    let exported = fixture.temp.path().join("verify-repo-bundle");
    manager
        .export_artifacts(&capsule.id, &exported)
        .expect("export bundle");

    let mut result = read_json(&exported.join("result.json"));
    result["changed_paths"] = serde_json::json!(["forged.txt"]);
    let encoded = serde_json::to_vec_pretty(&result).expect("encode forged result");
    fs::write(exported.join("result.json"), &encoded).expect("write forged result");
    let mut bundle = read_json(&exported.join("bundle.json"));
    for artifact in bundle["artifacts"]
        .as_array_mut()
        .expect("bundle artifacts")
    {
        if artifact["name"] == "result.json" {
            use sha2::{Digest, Sha256};
            let digest = hex::encode(Sha256::digest(&encoded));
            artifact["sha256"] = serde_json::Value::String(digest.clone());
            artifact["content_address"] = serde_json::Value::String(format!("sha256:{digest}"));
            artifact["bytes"] = serde_json::json!(encoded.len());
        }
    }
    write_json(&exported.join("bundle.json"), &bundle);

    assert!(
        verify_bundle(&exported, &VerifyOptions::default()).is_ok(),
        "offline checks alone cannot see the forged path list"
    );
    assert!(matches!(
        verify_bundle(
            &exported,
            &VerifyOptions::new(false, false, Some(fixture.repo.clone())),
        ),
        Err(Error::Verification(message)) if message.contains("changed paths")
    ));
}

#[test]
fn cli_verify_reports_verification_failures_with_their_own_error_kind() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("cli-verify");
    fs::write(capsule.workspace_path.join("shared.txt"), "cli verify\n").expect("edit capsule");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    let exported = fixture.temp.path().join("cli-verify-bundle");
    manager
        .export_artifacts(&capsule.id, &exported)
        .expect("export bundle");

    let output = capsule_cli(
        &fixture,
        ["verify", exported.to_str().expect("utf8 bundle path")],
    );
    assert_success(&output);
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse verify JSON");
    assert_eq!(value["capsule_id"], serde_json::json!(capsule.id));

    let output = capsule_cli(
        &fixture,
        [
            "verify",
            exported.to_str().expect("utf8 bundle path"),
            "--require-successful-evidence",
        ],
    );
    assert!(!output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("parse verify error JSON");
    assert_eq!(value["kind"], "verification");
}

#[test]
fn checkpoint_growth_is_refused_before_it_can_wedge_the_manifest() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("checkpoint-bound");
    let message = "m".repeat(16 * 1024 - 64);

    let mut accepted = 0_usize;
    let mut refusal = None;
    for round in 0..400 {
        fs::write(
            capsule.workspace_path.join(format!("file-{round}.txt")),
            format!("change {round}\n"),
        )
        .expect("workspace edit");
        match manager.checkpoint(
            &capsule.id,
            CheckpointOptions::new(message.clone(), test_author()),
        ) {
            Ok(_) => accepted += 1,
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }

    let refusal = refusal.expect("checkpoint growth must eventually be refused");
    assert!(
        matches!(&refusal, Error::InvalidInput(message)
            if message.contains("manifest") || message.contains("checkpoints")),
        "unexpected refusal after {accepted} accepted checkpoint(s): {refusal:?}"
    );
    assert!(accepted > 0, "no checkpoint was ever accepted");

    // The refusal must leave the capsule exactly as usable as before: still
    // active, still sealable, with the rejected checkpoint absent from both the
    // manifest and the branch.
    let after = manager.show(&capsule.id).expect("capsule survives refusal");
    assert_eq!(after.state, CapsuleState::Active);
    assert_eq!(after.checkpoints.len(), accepted);
    assert!(after.checkpoint.is_none());
    assert_eq!(
        manager.status(&capsule.id).expect("status").health,
        CapsuleHealth::Healthy
    );
    assert_eq!(
        git_text(&capsule.workspace_path, ["rev-parse", "HEAD"]),
        after
            .checkpoints
            .last()
            .expect("at least one checkpoint")
            .commit,
        "the refused checkpoint must not have advanced the branch"
    );
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("a capsule that refused a checkpoint must still seal");
}

#[test]
fn evidence_payload_is_bounded_before_it_can_wedge_the_manifest() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("evidence-bound");
    let summary = "s".repeat(64 * 1024);
    for _ in 0..3 {
        manager
            .add_evidence(
                &capsule.id,
                EvidenceInput::new("x".to_owned(), 0).with_summary(summary.clone()),
            )
            .expect("bounded evidence");
    }
    assert!(matches!(
        manager.add_evidence(
            &capsule.id,
            EvidenceInput::new("x".to_owned(), 0).with_summary(summary),
        ),
        Err(Error::InvalidInput(message)) if message.contains("evidence payload")
    ));
    assert_eq!(
        manager.show(&capsule.id).expect("capsule").evidence.len(),
        3
    );
}

#[test]
fn concurrent_managers_serialize_lifecycle_operations_safely() {
    let fixture = Fixture::new();
    let workers: Vec<_> = (0..4)
        .map(|index| {
            let repo = fixture.repo.clone();
            let state = fixture.state.clone();
            std::thread::spawn(move || {
                let manager = CapsuleManager::open(&state).expect("open concurrent manager");
                let mut options = CreateOptions::new(&repo);
                options.label = Some(format!("worker-{index}"));
                let capsule = manager.create(options).expect("concurrent create");
                fs::write(
                    capsule.workspace_path.join(format!("file-{index}.txt")),
                    "concurrent content\n",
                )
                .expect("concurrent edit");
                manager
                    .close(&capsule.id, CloseOptions::default())
                    .expect("concurrent close");
                capsule.id
            })
        })
        .collect();
    let ids: Vec<String> = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker thread"))
        .collect();
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        4,
        "concurrent creates must produce unique capsule identities"
    );

    let manager = fixture.manager();
    assert_eq!(manager.list().expect("list").len(), 4);
    for id in &ids {
        assert_eq!(
            manager.result(id).expect("sealed result").kind,
            ResultKind::Patch
        );
    }

    let contested = ids[0].clone();
    let droppers: Vec<_> = (0..2)
        .map(|_| {
            let state = fixture.state.clone();
            let id = contested.clone();
            std::thread::spawn(move || {
                CapsuleManager::open(&state)
                    .expect("open dropper manager")
                    .drop_capsule(&id, false)
                    .expect("racing drop must be idempotent")
                    .state
            })
        })
        .collect();
    for dropper in droppers {
        assert_eq!(
            dropper.join().expect("dropper thread"),
            CapsuleState::Dropped
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn stress_campaign_parallel_candidates_export_verify_select_and_cleanup() {
    const CANDIDATES: usize = 12;

    let fixture = Fixture::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(CANDIDATES));
    let receipt_root = fixture.temp.path().join("stress-receipts");
    fs::create_dir(&receipt_root).expect("create receipt root");

    let workers: Vec<_> = (0..CANDIDATES)
        .map(|index| {
            let barrier = barrier.clone();
            let repo = fixture.repo.clone();
            let state = fixture.state.clone();
            let receipt = receipt_root.join(format!("candidate-{index}"));
            std::thread::spawn(move || {
                let manager = CapsuleManager::open(&state).expect("open stress manager");
                let mut options = CreateOptions::new(&repo);
                options.label = Some(format!("stress-candidate-{index}"));
                options.links = BTreeMap::from([
                    ("campaign".to_owned(), "parallel-selection".to_owned()),
                    ("candidate".to_owned(), index.to_string()),
                ]);
                let capsule = manager.create(options).expect("create candidate");

                barrier.wait();
                fs::write(
                    capsule.workspace_path.join("shared.txt"),
                    format!("candidate-{index}\n"),
                )
                .expect("edit contested path");
                fs::write(
                    capsule
                        .workspace_path
                        .join(format!("candidate-{index}.txt")),
                    format!("isolated candidate {index}\n"),
                )
                .expect("write candidate path");

                if index % 3 == 0 {
                    manager
                        .checkpoint(
                            &capsule.id,
                            CheckpointOptions::new(
                                format!("candidate {index} checkpoint"),
                                test_author(),
                            ),
                        )
                        .expect("checkpoint candidate");
                    fs::write(
                        capsule
                            .workspace_path
                            .join(format!("post-checkpoint-{index}.txt")),
                        "work continued after checkpoint\n",
                    )
                    .expect("continue after checkpoint");
                }

                manager
                    .add_evidence(
                        &capsule.id,
                        EvidenceInput::new(format!("verify-candidate-{index}"), 0)
                            .with_summary("deterministic stress evidence".to_owned()),
                    )
                    .expect("record evidence");
                manager
                    .close(&capsule.id, CloseOptions::new(true, false))
                    .expect("seal candidate");
                manager
                    .export_artifacts(&capsule.id, &receipt)
                    .expect("export candidate receipt");
                let report = verify_bundle(&receipt, &VerifyOptions::new(true, false, Some(repo)))
                    .expect("verify candidate receipt");
                assert_eq!(report.capsule_id, capsule.id);
                (index, capsule.id, receipt)
            })
        })
        .collect();

    let mut candidates: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("stress worker"))
        .collect();
    candidates.sort_by_key(|candidate| candidate.0);

    let manager = fixture.manager();
    let before_selection = manager.metrics().expect("campaign metrics");
    assert_eq!(before_selection.capsules, CANDIDATES as u64);
    assert_eq!(before_selection.live_capsules, CANDIDATES as u64);
    assert_eq!(before_selection.sealed_results, CANDIDATES as u64);

    let selected = &candidates[7];
    manager
        .integrate(
            &selected.1,
            &IntegrateOptions::new(fixture.repo.clone(), test_author())
                .with_message("select stress candidate 7".to_owned()),
        )
        .expect("integrate selected candidate");
    assert_eq!(
        fs::read_to_string(fixture.repo.join("shared.txt")).expect("read selected result"),
        "candidate-7\n"
    );
    assert!(fixture.repo.join("candidate-7.txt").is_file());
    for index in 0..CANDIDATES {
        if index != 7 {
            assert!(!fixture.repo.join(format!("candidate-{index}.txt")).exists());
        }
    }

    for (_, _, receipt) in &candidates {
        verify_bundle(
            receipt,
            &VerifyOptions::new(true, false, Some(fixture.repo.clone())),
        )
        .expect("every candidate remains independently verifiable");
    }

    let droppers: Vec<_> = candidates
        .iter()
        .map(|(_, id, _)| {
            let state = fixture.state.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                CapsuleManager::open(state)
                    .expect("open cleanup manager")
                    .drop_capsule(&id, false)
                    .expect("drop sealed candidate")
                    .state
            })
        })
        .collect();
    for dropper in droppers {
        assert_eq!(
            dropper.join().expect("cleanup worker"),
            CapsuleState::Dropped
        );
    }

    let after_cleanup = manager.metrics().expect("post-campaign metrics");
    assert_eq!(after_cleanup.capsules, CANDIDATES as u64);
    assert_eq!(after_cleanup.live_capsules, 0);
    assert_eq!(after_cleanup.sealed_results, CANDIDATES as u64);
    assert_eq!(
        after_cleanup.states.get("dropped"),
        Some(&(CANDIDATES as u64))
    );
    assert!(manager.recover().expect("idempotent recovery").is_empty());
    for (_, _, receipt) in candidates {
        verify_bundle(&receipt, &VerifyOptions::default())
            .expect("receipt survives concurrent cleanup");
    }
}

#[test]
fn current_evidence_binds_to_the_complete_patch_and_stale_evidence_is_rejected() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("current-evidence");
    fs::write(capsule.workspace_path.join("shared.txt"), "tested\n").expect("edit");
    let evidence = manager
        .add_evidence(
            &capsule.id,
            EvidenceInput::new("caller-ran-tests".to_owned(), 0),
        )
        .expect("record bound claim");
    assert!(evidence.patch_sha256.is_some());
    fs::write(capsule.workspace_path.join("shared.txt"), "changed later\n").expect("stale edit");
    assert!(matches!(
        manager.close(
            &capsule.id,
            CloseOptions::new(false, true),
        ),
        Err(Error::InvalidInput(message)) if message.contains("current successful evidence")
    ));
    let current = manager
        .add_evidence(
            &capsule.id,
            EvidenceInput::new("caller-ran-tests-again".to_owned(), 0),
        )
        .expect("record current claim");
    let result = manager
        .close(&capsule.id, CloseOptions::new(false, true))
        .expect("seal current claim");
    assert_eq!(
        current.patch_sha256.as_deref(),
        Some(result.patch_sha256.as_str())
    );

    let receipt = fixture.temp.path().join("current-evidence-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    verify_bundle(
        &receipt,
        &VerifyOptions::new(false, true, Some(fixture.repo.clone())),
    )
    .expect("verify current evidence against seal");
}

#[test]
fn state_v3_migration_is_dry_run_then_backup_first_and_marks_evidence_unbound() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("migration");
    manager
        .add_evidence(
            &capsule.id,
            EvidenceInput::new("legacy claim".to_owned(), 0),
        )
        .expect("evidence");
    fs::write(capsule.workspace_path.join("shared.txt"), "legacy\n").expect("edit");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    for path in [
        manifest_path(&fixture, &capsule.id),
        result_path(&fixture, &capsule.id),
    ] {
        let mut value = read_json(&path);
        value["schema_version"] = serde_json::json!(3);
        for evidence in value["evidence"].as_array_mut().expect("evidence array") {
            evidence
                .as_object_mut()
                .expect("evidence object")
                .remove("patch_sha256");
        }
        write_json(&path, &value);
    }

    let before = fs::read(manifest_path(&fixture, &capsule.id)).expect("before dry run");
    let dry = manager
        .migrate_state_v3(None::<&Path>, false)
        .expect("dry-run migration");
    assert!(!dry.applied);
    assert!(dry.backup_directory.is_none());
    assert_eq!(dry.capsules, 1);
    assert_eq!(dry.results, 1);
    assert_eq!(dry.unbound_evidence, 2);
    assert_eq!(
        fs::read(manifest_path(&fixture, &capsule.id)).expect("after dry run"),
        before
    );
    assert!(manager.migrate_state_v3(None::<&Path>, true).is_err());

    let backup = fixture.temp.path().join("migration-backup");
    let report = manager
        .migrate_state_v3(Some(&backup), true)
        .expect("apply migration");
    assert!(report.applied);
    assert!(backup.join("backup.json").is_file());
    assert_eq!(
        read_json(
            &backup
                .join("capsules")
                .join(&capsule.id)
                .join("capsule.json")
        )["schema_version"],
        3
    );
    let migrated = manager.show(&capsule.id).expect("read migrated state");
    assert_eq!(migrated.schema_version, 4);
    assert_eq!(migrated.evidence[0].patch_sha256, None);
    assert_eq!(report.unbound_evidence, 2);
    assert_eq!(
        manager
            .result(&capsule.id)
            .expect("migrated seal")
            .schema_version,
        4
    );
}

#[test]
fn migration_dry_run_rejects_backup_at_library_and_cli_boundaries() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let backup = fixture.temp.path().join("dry-backup");
    assert!(matches!(
        manager.migrate_state_v3(Some(&backup), false),
        Err(Error::InvalidInput(message)) if message.contains("apply=true")
    ));
    assert!(!backup.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args([
            "--home",
            fixture.state.to_str().expect("utf8 state path"),
            "--json",
            "state",
            "migrate",
            "--dry-run",
            "--backup",
            backup.to_str().expect("utf8 backup path"),
        ])
        .output()
        .expect("run capsule CLI");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("parse CLI error JSON");
    assert_eq!(error["kind"], "invalid_input");
    assert!(!backup.exists());
}

#[test]
fn migration_rejects_adversarial_pair_mismatches_before_backup_or_write() {
    for (target, field, value) in [
        ("result", "label", serde_json::json!("different")),
        ("manifest-result", "changed_paths", serde_json::json!(999)),
        ("manifest-result", "sealed_at_unix", serde_json::json!(0)),
    ] {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let capsule = fixture.create(&format!("mismatch-{field}"));
        fs::write(capsule.workspace_path.join("shared.txt"), "changed\n").expect("edit");
        manager
            .close(&capsule.id, CloseOptions::default())
            .expect("close");
        for path in [
            manifest_path(&fixture, &capsule.id),
            result_path(&fixture, &capsule.id),
        ] {
            let mut json = read_json(&path);
            json["schema_version"] = serde_json::json!(3);
            write_json(&path, &json);
        }
        let changed_path = if target == "result" {
            result_path(&fixture, &capsule.id)
        } else {
            manifest_path(&fixture, &capsule.id)
        };
        let mut changed = read_json(&changed_path);
        if target == "result" {
            changed[field] = value;
        } else {
            changed["result"][field] = value;
        }
        write_json(&changed_path, &changed);
        let before_manifest = fs::read(manifest_path(&fixture, &capsule.id)).expect("manifest");
        let before_result = fs::read(result_path(&fixture, &capsule.id)).expect("result");
        let backup = fixture.temp.path().join("must-not-exist");
        assert!(manager.migrate_state_v3(Some(&backup), true).is_err());
        assert!(!backup.exists());
        assert_eq!(
            fs::read(manifest_path(&fixture, &capsule.id)).expect("manifest unchanged"),
            before_manifest
        );
        assert_eq!(
            fs::read(result_path(&fixture, &capsule.id)).expect("result unchanged"),
            before_result
        );
    }
}

#[test]
fn migration_rejects_mixed_current_and_legacy_pairs() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("mixed-schema");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    let result = result_path(&fixture, &capsule.id);
    let mut legacy = read_json(&result);
    legacy["schema_version"] = serde_json::json!(3);
    write_json(&result, &legacy);
    let backup = fixture.temp.path().join("must-not-exist");
    assert!(matches!(
        manager.migrate_state_v3(Some(&backup), true),
        Err(Error::UnsafeState(message)) if message.contains("mixed-schema")
    ));
    assert!(!backup.exists());
}

#[test]
fn opening_store_does_not_recover_a_live_migration_before_global_lock() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    drop(manager);
    let lock_path = fixture.state.join("locks/global.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open global lock");
    lock.lock_exclusive().expect("hold global lock");
    let journal = fixture.state.join(".migration-v3-v4");
    fs::create_dir(&journal).expect("create live staging journal");

    let mut child = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args([
            "--home",
            fixture.state.to_str().expect("utf8 state path"),
            "--json",
            "list",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start concurrent opener");
    thread::sleep(Duration::from_millis(200));
    assert!(child.try_wait().expect("poll opener").is_none());
    assert!(
        journal.is_dir(),
        "live journal must remain while lock is held"
    );

    lock.unlock().expect("release global lock");
    let output = child.wait_with_output().expect("finish opener");
    assert_success(&output);
    assert!(
        !journal.exists(),
        "opener recovers only after taking the lock"
    );
}

#[test]
fn migration_recovery_distinguishes_active_rollback_from_committed_cleanup() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("migration-recovery");
    let manifest = manifest_path(&fixture, &capsule.id);
    let original = fs::read(&manifest).expect("original manifest");
    drop(manager);

    let active = fixture.state.join(".migration-v3-v4");
    fs::create_dir(&active).expect("active journal");
    fs::write(active.join("0.json"), &original).expect("journal original");
    write_json(
        &active.join("targets.json"),
        &serde_json::json!([format!("capsules/{}/capsule.json", capsule.id)]),
    );
    fs::write(&manifest, b"partially migrated").expect("partial target write");
    drop(fixture.manager());
    assert_eq!(fs::read(&manifest).expect("rolled back manifest"), original);
    assert!(!active.exists());

    let committed = fixture.state.join(".migration-v3-v4-committed-cleanup");
    fs::create_dir(&committed).expect("committed cleanup");
    fs::write(committed.join("0.json"), b"partially deleted journal").expect("partial cleanup");
    drop(fixture.manager());
    assert!(!committed.exists());
    assert_eq!(
        fs::read(&manifest).expect("committed target retained"),
        original
    );

    drop(fixture.manager());
    assert_eq!(fs::read(&manifest).expect("idempotent reopen"), original);
}

#[test]
fn migration_recovery_fails_closed_when_both_namespaces_exist() {
    let fixture = Fixture::new();
    drop(fixture.manager());
    let active = fixture.state.join(".migration-v3-v4");
    let committed = fixture.state.join(".migration-v3-v4-committed-cleanup");
    fs::create_dir(&active).expect("active journal");
    fs::create_dir(&committed).expect("committed journal");
    assert!(matches!(
        CapsuleManager::open(&fixture.state),
        Err(Error::UnsafeState(message)) if message.contains("both active and committed")
    ));
    assert!(active.is_dir());
    assert!(committed.is_dir());
}

#[test]
fn migration_rejects_result_patch_corruption_without_writing_or_backup() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("migration-corrupt");
    fs::write(capsule.workspace_path.join("shared.txt"), "changed\n").expect("edit");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    for path in [
        manifest_path(&fixture, &capsule.id),
        result_path(&fixture, &capsule.id),
    ] {
        let mut value = read_json(&path);
        value["schema_version"] = serde_json::json!(3);
        write_json(&path, &value);
    }
    fs::write(
        fixture
            .state
            .join("capsules")
            .join(&capsule.id)
            .join("result.patch"),
        b"corrupt",
    )
    .expect("corrupt patch");
    let before = fs::read(result_path(&fixture, &capsule.id)).expect("legacy result");
    let backup = fixture.temp.path().join("must-not-exist");
    assert!(manager.migrate_state_v3(Some(&backup), true).is_err());
    assert!(!backup.exists());
    assert_eq!(
        fs::read(result_path(&fixture, &capsule.id)).expect("unchanged result"),
        before
    );
}

#[test]
fn authenticated_verification_cannot_combine_different_bundle_snapshots() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let first = fixture.create("signed-first");
    let second = fixture.create("signed-second");
    manager
        .close(&first.id, CloseOptions::default())
        .expect("close first");
    manager
        .close(&second.id, CloseOptions::default())
        .expect("close second");
    let first_receipt = fixture.temp.path().join("first-receipt");
    let second_receipt = fixture.temp.path().join("second-receipt");
    manager
        .export_artifacts(&first.id, &first_receipt)
        .expect("export first");
    manager
        .export_artifacts(&second.id, &second_receipt)
        .expect("export second");

    let seed = [7_u8; 32];
    let first_bytes = fs::read(first_receipt.join("bundle.json")).expect("first snapshot");
    let signature = sign_bundle_bytes(&first_bytes, &seed);
    let trusted = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    let report = verify_authenticated_bundle(
        &first_receipt,
        &signature,
        &trusted,
        &VerifyOptions::default(),
    )
    .expect("authenticate exact first snapshot");
    assert!(report.signature_authenticated);
    assert!(
        verify_authenticated_bundle(
            &second_receipt,
            &signature,
            &trusted,
            &VerifyOptions::default(),
        )
        .is_err(),
        "signature from one valid snapshot must not authenticate another valid snapshot"
    );
}

#[test]
fn current_result_requires_ignored_digest_and_canonical_lowercase_hashes() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("result-canonicality");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close capsule");
    let receipt = fixture.temp.path().join("canonicality-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export receipt");

    let result_path = receipt.join("result.json");
    let bundle_path = receipt.join("bundle.json");
    for mutation in ["missing_ignored_digest", "uppercase_ignored_digest"] {
        let original_result = fs::read(&result_path).expect("original result");
        let original_bundle = fs::read(&bundle_path).expect("original bundle");
        let mut result: serde_json::Value =
            serde_json::from_slice(&original_result).expect("decode result");
        if mutation == "missing_ignored_digest" {
            assert!(
                result
                    .as_object_mut()
                    .expect("result object")
                    .remove("ignored_content_sha256")
                    .is_some(),
                "sealed result had no ignored-content digest"
            );
        } else {
            let digest = result["ignored_content_sha256"]
                .as_str()
                .expect("ignored-content digest");
            result["ignored_content_sha256"] = serde_json::json!(format!("A{}", &digest[1..]));
        }
        let mut changed = serde_json::to_vec_pretty(&result).expect("encode changed result");
        changed.push(b'\n');
        fs::write(&result_path, &changed).expect("write changed result");
        let mut bundle: serde_json::Value =
            serde_json::from_slice(&original_bundle).expect("decode bundle");
        let descriptor = bundle["artifacts"]
            .as_array_mut()
            .expect("artifact descriptors")
            .iter_mut()
            .find(|descriptor| descriptor["name"] == "result.json")
            .expect("result descriptor");
        let digest = hex::encode(Sha256::digest(&changed));
        descriptor["bytes"] = serde_json::json!(changed.len());
        descriptor["sha256"] = serde_json::json!(&digest);
        descriptor["content_address"] = serde_json::json!(format!("sha256:{digest}"));
        fs::write(
            &bundle_path,
            serde_json::to_vec_pretty(&bundle).expect("encode bundle"),
        )
        .expect("write bundle");

        let verification = verify_bundle(&receipt, &VerifyOptions::default());
        assert!(
            verification.is_err(),
            "verification accepted {mutation}: {verification:?}; changed result: {}",
            String::from_utf8_lossy(&changed)
        );
        fs::write(&result_path, &original_result).expect("restore result");
        fs::write(&bundle_path, &original_bundle).expect("restore bundle");
    }
}

#[test]
fn detached_signature_covers_exact_bundle_bytes_and_requires_trusted_key() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("signature");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    let receipt = fixture.temp.path().join("signed-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    let bundle = fs::read(receipt.join("bundle.json")).expect("bundle bytes");
    let seed = [7_u8; 32];
    let signature = sign_bundle_bytes(&bundle, &seed);
    let trusted = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    verify_bundle_signature_bytes(&bundle, &signature, &trusted).expect("valid signature");
    let mut changed = bundle.clone();
    changed.push(b' ');
    assert!(verify_bundle_signature_bytes(&changed, &signature, &trusted).is_err());
    let other = SigningKey::from_bytes(&[9_u8; 32])
        .verifying_key()
        .to_bytes();
    assert!(verify_bundle_signature_bytes(&bundle, &signature, &other).is_err());
}

#[test]
fn cli_keygen_sign_and_verify_report_authentication() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("cli-keygen");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    let receipt = fixture.temp.path().join("keygen-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    let private = fixture.temp.path().join("private.seed");
    let public = fixture.temp.path().join("public.key");
    let signature = fixture.temp.path().join("bundle.sig");
    assert_success(&capsule_cli(
        &fixture,
        [
            std::ffi::OsStr::new("keygen"),
            std::ffi::OsStr::new("--private-key"),
            private.as_os_str(),
            std::ffi::OsStr::new("--public-key"),
            public.as_os_str(),
        ],
    ));
    assert_eq!(fs::metadata(&private).expect("private metadata").len(), 32);
    assert_eq!(fs::metadata(&public).expect("public metadata").len(), 32);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&private)
                .expect("private metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(
        !capsule_cli(
            &fixture,
            [
                std::ffi::OsStr::new("keygen"),
                std::ffi::OsStr::new("--private-key"),
                private.as_os_str(),
                std::ffi::OsStr::new("--public-key"),
                public.as_os_str(),
            ],
        )
        .status
        .success()
    );
    assert_success(&capsule_cli(
        &fixture,
        [
            std::ffi::OsStr::new("sign"),
            receipt.as_os_str(),
            std::ffi::OsStr::new("--private-key"),
            private.as_os_str(),
            std::ffi::OsStr::new("--output"),
            signature.as_os_str(),
        ],
    ));
    let output = capsule_cli(
        &fixture,
        [
            std::ffi::OsStr::new("verify"),
            receipt.as_os_str(),
            std::ffi::OsStr::new("--signature"),
            signature.as_os_str(),
            std::ffi::OsStr::new("--trusted-public-key"),
            public.as_os_str(),
        ],
    );
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("verify JSON");
    assert_eq!(report["signature_authenticated"], true);
}

#[test]
fn keygen_reports_partial_publication_and_sign_never_overwrites() {
    let fixture = Fixture::new();
    let private = fixture.temp.path().join("existing.seed");
    let public = fixture.temp.path().join("published.key");
    fs::write(&private, [1_u8; 32]).expect("existing private path");
    let output = capsule_cli(
        &fixture,
        [
            std::ffi::OsStr::new("keygen"),
            std::ffi::OsStr::new("--private-key"),
            private.as_os_str(),
            std::ffi::OsStr::new("--public-key"),
            public.as_os_str(),
        ],
    );
    assert!(!output.status.success());
    assert!(public.is_file(), "harmless public key remains published");
    assert_eq!(fs::read(&private).expect("private untouched"), [1_u8; 32]);
    // JSON mode escapes backslashes, so a raw-byte match would miss a Windows
    // path. Decode the envelope and assert against the real message.
    let reported: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("keygen error JSON");
    assert!(
        reported["error"]
            .as_str()
            .expect("keygen error message")
            .contains(public.to_string_lossy().as_ref()),
        "{reported}"
    );

    let manager = fixture.manager();
    let capsule = fixture.create("signature-no-overwrite");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    let receipt = fixture.temp.path().join("signature-no-overwrite-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    let signature = fixture.temp.path().join("existing.sig");
    fs::write(&signature, b"keep me").expect("existing signature");
    assert!(sign_bundle(&receipt, &[2_u8; 32], &signature).is_err());
    assert_eq!(
        fs::read(&signature).expect("signature untouched"),
        b"keep me"
    );
}

#[test]
fn key_inputs_require_exact_length_without_trailing_bytes() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("key-length");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    let receipt = fixture.temp.path().join("key-length-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    let private = fixture.temp.path().join("long.seed");
    fs::write(&private, [3_u8; 33]).expect("trailing private byte");
    let output = capsule_cli(
        &fixture,
        [
            std::ffi::OsStr::new("sign"),
            receipt.as_os_str(),
            std::ffi::OsStr::new("--private-key"),
            private.as_os_str(),
            std::ffi::OsStr::new("--output"),
            fixture.temp.path().join("unused.sig").as_os_str(),
        ],
    );
    assert!(!output.status.success());
}

#[cfg(unix)]
#[test]
fn key_inputs_reject_symlinks() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("key-symlink");
    manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    let receipt = fixture.temp.path().join("key-symlink-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    let target = fixture.temp.path().join("target.seed");
    let link = fixture.temp.path().join("linked.seed");
    fs::write(&target, [4_u8; 32]).expect("key target");
    symlink(&target, &link).expect("key symlink");
    let output = capsule_cli(
        &fixture,
        [
            std::ffi::OsStr::new("sign"),
            receipt.as_os_str(),
            std::ffi::OsStr::new("--private-key"),
            link.as_os_str(),
            std::ffi::OsStr::new("--output"),
            fixture.temp.path().join("linked.sig").as_os_str(),
        ],
    );
    assert!(!output.status.success());
}

// macOS enforces UTF-8 filenames (EILSEQ), so the fixture itself cannot be
// created there. The encoding under test stays platform-independent.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_ignored_names_and_symlink_targets_hash_losslessly() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("non-utf8-ignored");
    fs::write(capsule.workspace_path.join(".gitignore"), b"ignored-*\n").expect("ignore rule");
    let raw_name = b"ignored-\xff".to_vec();
    let ignored = capsule
        .workspace_path
        .join(std::ffi::OsString::from_vec(raw_name.clone()));
    fs::write(&ignored, b"ignored bytes").expect("write ignored non-UTF-8 file");
    let link = capsule.workspace_path.join("ignored-link");
    symlink(std::ffi::OsString::from_vec(b"target-\xfe".to_vec()), &link)
        .expect("write non-UTF-8 symlink target");

    let result = manager
        .close(&capsule.id, CloseOptions::default())
        .expect("hash ignored native names");
    assert!(result.ignored_paths.contains(&GitPath::UnixBytes {
        unix_bytes_hex: hex::encode(raw_name),
    }));
    assert!(result.ignored_content_sha256.is_some());
}

#[test]
fn git_path_raw_encoding_is_strictly_canonical() {
    let valid: GitPath = serde_json::from_str(r#"{"unix_bytes_hex":"ff"}"#).expect("raw path");
    assert!(valid.is_valid_encoding());
    for invalid in [
        r#"{"unix_bytes_hex":"FF"}"#,
        r#"{"unix_bytes_hex":"666f6f"}"#,
        r#"{"unix_bytes_hex":"f"}"#,
        r#"{"unix_bytes_hex":"00"}"#,
        r#"{"unix_bytes_hex":"ff","extra":true}"#,
    ] {
        assert!(
            serde_json::from_str::<GitPath>(invalid).is_err(),
            "{invalid}"
        );
    }
}

// macOS enforces UTF-8 filenames (EILSEQ), so the fixture itself cannot be
// created there. The encoding under test stays platform-independent.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_git_inventory_paths_round_trip_losslessly() {
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("non-utf8");
    let raw = b"bad-\xff.txt".to_vec();
    fs::write(
        capsule
            .workspace_path
            .join(std::ffi::OsString::from_vec(raw.clone())),
        b"bytes\n",
    )
    .expect("write non-UTF-8 path");
    let expected = GitPath::UnixBytes {
        unix_bytes_hex: hex::encode(&raw),
    };
    assert_eq!(
        manager.status(&capsule.id).expect("status").changed_paths,
        vec![expected.clone()]
    );
    let result = manager
        .close(&capsule.id, CloseOptions::default())
        .expect("close");
    assert_eq!(result.changed_paths, vec![expected.clone()]);
    let encoded = serde_json::to_vec(&result).expect("encode result");
    let decoded: change_capsule::CapsuleResult =
        serde_json::from_slice(&encoded).expect("decode result");
    assert_eq!(decoded.changed_paths, vec![expected]);
    let receipt = fixture.temp.path().join("non-utf8-receipt");
    manager
        .export_artifacts(&capsule.id, &receipt)
        .expect("export");
    verify_bundle(
        receipt,
        &VerifyOptions::new(false, false, Some(fixture.repo.clone())),
    )
    .expect("verify non-UTF-8 inventory");
}

#[test]
fn concurrent_policy_limit_is_linearizable_under_create_pressure() {
    const CONTENDERS: usize = 16;
    const LIMIT: usize = 5;

    let fixture = Fixture::new();
    let manager = fixture.manager();
    let mut policy = manager.policy().expect("read policy");
    policy.max_live_capsules = Some(LIMIT as u64);
    manager.set_policy(policy).expect("set live limit");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(CONTENDERS));
    let contenders: Vec<_> = (0..CONTENDERS)
        .map(|index| {
            let barrier = barrier.clone();
            let repo = fixture.repo.clone();
            let state = fixture.state.clone();
            std::thread::spawn(move || {
                let manager = CapsuleManager::open(state).expect("open contender manager");
                barrier.wait();
                let mut options = CreateOptions::new(repo);
                options.label = Some(format!("quota-contender-{index}"));
                manager.create(options)
            })
        })
        .collect();

    let mut admitted = Vec::new();
    let mut rejected = 0;
    for contender in contenders {
        match contender.join().expect("quota contender") {
            Ok(capsule) => admitted.push(capsule.id),
            Err(Error::PolicyViolation(message)) if message.contains("live capsules") => {
                rejected += 1;
            }
            Err(error) => panic!("unexpected quota result: {error}"),
        }
    }
    assert_eq!(admitted.len(), LIMIT);
    assert_eq!(rejected, CONTENDERS - LIMIT);
    assert_eq!(
        admitted
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        LIMIT
    );
    assert_eq!(manager.list().expect("list admitted capsules").len(), LIMIT);
    assert_eq!(
        manager.metrics().expect("quota metrics").live_capsules,
        LIMIT as u64
    );

    let cleanup: Vec<_> = admitted
        .into_iter()
        .map(|id| {
            let state = fixture.state.clone();
            std::thread::spawn(move || {
                CapsuleManager::open(state)
                    .expect("open quota cleanup manager")
                    .drop_capsule(&id, true)
                    .expect("force-drop admitted contender")
            })
        })
        .collect();
    for worker in cleanup {
        assert_eq!(
            worker.join().expect("quota cleanup").state,
            CapsuleState::Dropped
        );
    }
    let metrics = manager.metrics().expect("final quota metrics");
    assert_eq!(metrics.capsules, LIMIT as u64);
    assert_eq!(metrics.live_capsules, 0);
}

fn manifest_path(fixture: &Fixture, id: &str) -> PathBuf {
    fixture.state.join("capsules").join(id).join("capsule.json")
}

fn result_path(fixture: &Fixture, id: &str) -> PathBuf {
    fixture.state.join("capsules").join(id).join("result.json")
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("read JSON")).expect("parse JSON")
}

fn write_json(path: &Path, value: &serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(value).expect("encode JSON")).expect("write JSON");
}

fn test_author() -> Author {
    Author::new("Test Agent".to_owned(), "agent@example.test".to_owned())
}

fn git_success<I, S>(directory: &Path, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("run git");
    assert_success(&output);
}

fn git_bytes<I, S>(directory: &Path, args: I) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("run git");
    assert_success(&output);
    output.stdout
}

fn git_text<I, S>(directory: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("run git");
    assert_success(&output);
    String::from_utf8(output.stdout)
        .expect("utf8 git output")
        .trim()
        .to_owned()
}

fn capsule_cli<I, S>(fixture: &Fixture, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args([
            "--home",
            fixture.state.to_str().expect("utf8 state path"),
            "--json",
        ])
        .args(args)
        .output()
        .expect("run capsule CLI")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "git failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
