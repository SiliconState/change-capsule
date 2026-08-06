use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use change_capsule::{
    Author, CapsuleHealth, CapsuleManager, CapsuleState, CheckpointOptions, CloseOptions,
    CreateOptions, Error, EvidenceInput, IntegrateOptions, ResultKind,
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
        git_success(&repo, ["init", "-b", "main"]);
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
            CheckpointOptions {
                message: "first implementation".to_owned(),
                author: test_author(),
            },
        )
        .expect("checkpoint first");
    assert_eq!(checkpoint.commit.len(), 40);
    manager
        .add_evidence(
            &first.id,
            EvidenceInput {
                command: "cargo test".to_owned(),
                exit_code: 0,
                summary: Some("all tests passed".to_owned()),
            },
        )
        .expect("first evidence");
    let first_result = manager
        .close(
            &first.id,
            CloseOptions {
                require_successful_evidence: true,
            },
        )
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
            &IntegrateOptions {
                target: fixture.repo.clone(),
                message: Some("select first approach".to_owned()),
                author: test_author(),
            },
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
            &IntegrateOptions {
                target: fixture.repo.clone(),
                message: None,
                author: test_author(),
            },
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
            CheckpointOptions {
                message: "recover this checkpoint".to_owned(),
                author: test_author(),
            },
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
fn result_seal_covers_provenance_and_rejects_metadata_tampering() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("provenance");
    fs::write(capsule.workspace_path.join("shared.txt"), "sealed\n").expect("edit capsule");
    let checkpoint = manager
        .checkpoint(
            &capsule.id,
            CheckpointOptions {
                message: "preserve provenance".to_owned(),
                author: test_author(),
            },
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
            &IntegrateOptions {
                target: fixture.repo.clone(),
                message: Some("expected integration".to_owned()),
                author: test_author(),
            },
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
            CheckpointOptions {
                message: "hook-free checkpoint".to_owned(),
                author: test_author(),
            },
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
fn sparse_checkout_is_rejected_instead_of_treating_absent_files_as_deletions() {
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
    let manager = fixture.manager();
    let capsule = fixture.create("sparse");
    git_success(
        &capsule.workspace_path,
        ["sparse-checkout", "set", "--skip-checks", "shared.txt"],
    );

    assert!(matches!(
        manager.status(&capsule.id),
        Err(Error::InvalidInput(message)) if message.contains("sparse-checkout")
    ));
    assert!(matches!(
        manager.close(&capsule.id, CloseOptions::default()),
        Err(Error::InvalidInput(message)) if message.contains("sparse-checkout")
    ));
}

#[test]
fn skip_worktree_index_entries_are_rejected_as_incomplete_snapshots() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("skip-worktree");
    git_success(
        &capsule.workspace_path,
        ["update-index", "--skip-worktree", "shared.txt"],
    );

    assert!(matches!(
        manager.status(&capsule.id),
        Err(Error::InvalidInput(message)) if message.contains("skip-worktree")
    ));
}

#[test]
fn assume_unchanged_index_entries_are_rejected_as_incomplete_snapshots() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = fixture.create("assume-unchanged");
    git_success(
        &capsule.workspace_path,
        ["update-index", "--assume-unchanged", "shared.txt"],
    );

    assert!(matches!(
        manager.status(&capsule.id),
        Err(Error::InvalidInput(message)) if message.contains("assume-unchanged")
    ));
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
    Author {
        name: "Test Agent".to_owned(),
        email: "agent@example.test".to_owned(),
    }
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "git failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
