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
    _temp: TempDir,
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
        Self {
            _temp: temp,
            repo,
            state,
        }
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
    let manifest = fixture
        .state
        .join("capsules")
        .join(&capsule.id)
        .join("capsule.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
            .expect("parse manifest");
    value["state"] = serde_json::Value::String("creating".to_owned());
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
            .state,
        CapsuleState::Active
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "git failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
