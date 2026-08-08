//! Unit tests for manager internals.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};

use super::{
    CLOSE_IGNORED_INVENTORY_TEST_HOOK, CapsuleManager, CloseIgnoredInventoryTestHook, CloseOptions,
    CreateOptions, IDEMPOTENT_CREATE_TEST_HOOK, IdempotentCreateTestStage,
    require_stable_close_snapshot,
};
use crate::error::Error;
use crate::git::Snapshot;
use crate::idempotency::{IdempotencyStatus, key_sha256};
use crate::model::{CapsuleState, GitPath};

fn snapshot(patch: &[u8], paths: &[&str]) -> Snapshot {
    Snapshot {
        patch: patch.to_vec(),
        changed_paths: paths
            .iter()
            .map(|path| GitPath::Utf8((*path).to_owned()))
            .collect(),
    }
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

static IDEMPOTENT_CREATE_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup_repository() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    let state = temporary.path().join("state");
    fs::create_dir(&repository).expect("create repository");
    git(&repository, &["init", "-b", "main"]);
    git(&repository, &["config", "core.autocrlf", "false"]);
    fs::write(repository.join("tracked.txt"), b"base\n").expect("write tracked file");
    git(&repository, &["add", "."]);
    git(
        &repository,
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
    (temporary, repository, state)
}

fn idempotent_options(repository: &Path) -> CreateOptions {
    CreateOptions::new(repository.to_path_buf())
        .with_base("HEAD")
        .with_label("idempotent crash recovery")
}

#[test]
fn idempotent_retry_resumes_reservation_only_and_manifest_only_windows() {
    let _serial = IDEMPOTENT_CREATE_TEST_SERIAL
        .lock()
        .expect("idempotent test serial lock");
    for stage in [
        IdempotentCreateTestStage::AfterReservation,
        IdempotentCreateTestStage::AfterManifest,
    ] {
        let (_temporary, repository, state) = setup_repository();
        let manager = CapsuleManager::open(&state).expect("manager");
        *IDEMPOTENT_CREATE_TEST_HOOK
            .lock()
            .expect("install idempotency hook") = Some(stage);
        assert!(matches!(
            manager.create_idempotent(idempotent_options(&repository), "crash:window"),
            Err(Error::UnsafeState(message)) if message.contains("injected")
        ));
        let before = manager
            .lookup_idempotency_key("crash:window")
            .expect("lookup interrupted reservation");
        let reserved_id = before.capsule_id.clone();
        match stage {
            IdempotentCreateTestStage::AfterReservation => {
                assert_eq!(before.status, IdempotencyStatus::Reserved);
                assert!(before.capsule.is_none());
            }
            IdempotentCreateTestStage::AfterManifest => {
                assert_eq!(before.status, IdempotencyStatus::Materialized);
                assert_eq!(
                    before.capsule.expect("creating capsule").state,
                    CapsuleState::Creating
                );
            }
        }
        let resumed = manager
            .create_idempotent(idempotent_options(&repository), "crash:window")
            .expect("resume exact reservation");
        assert_eq!(resumed.id, reserved_id);
        assert_eq!(resumed.state, CapsuleState::Active);
        assert_eq!(manager.list().expect("one capsule").len(), 1);
    }
}

#[test]
fn contradictory_git_state_orphans_reserved_identity_without_replacement() {
    let _serial = IDEMPOTENT_CREATE_TEST_SERIAL
        .lock()
        .expect("idempotent test serial lock");
    let (_temporary, repository, state) = setup_repository();
    let manager = CapsuleManager::open(&state).expect("manager");
    *IDEMPOTENT_CREATE_TEST_HOOK
        .lock()
        .expect("install idempotency hook") = Some(IdempotentCreateTestStage::AfterReservation);
    assert!(
        manager
            .create_idempotent(idempotent_options(&repository), "partial:git")
            .is_err()
    );
    let digest = key_sha256("partial:git").expect("key digest");
    let record = manager
        .store
        .read_idempotency_record(&digest)
        .expect("read reservation")
        .expect("reservation");
    manager
        .git
        .create_ref(
            &repository,
            &format!("refs/heads/capsule/{}", &record.capsule_id[4..]),
            &record.base_commit,
        )
        .expect("inject reserved branch only");
    let replay = manager
        .create_idempotent(idempotent_options(&repository), "partial:git")
        .expect("same identity becomes orphaned");
    assert_eq!(replay.id, record.capsule_id);
    assert_eq!(replay.state, CapsuleState::Orphaned);
    assert_eq!(manager.list().expect("one capsule").len(), 1);
    // Orphaning is a successful lifecycle transition, so it must leave a
    // record of why; otherwise the capsule is inexplicably stuck.
    let event = replay
        .audit_events
        .last()
        .expect("orphaning records an audit event");
    assert_eq!(event.state, Some(CapsuleState::Orphaned));
    assert_eq!(event.previous_state, Some(CapsuleState::Creating));
    assert!(
        event.attributes["reason"].contains("contradictory"),
        "{:?}",
        event.attributes
    );
    let again = manager
        .create_idempotent(idempotent_options(&repository), "partial:git")
        .expect("orphan replay");
    assert_eq!(again.id, replay.id);
    assert_eq!(again.state, CapsuleState::Orphaned);
    // A replay of an orphan must not append a second orphaning event.
    assert_eq!(again.audit_events.len(), replay.audit_events.len());
}

#[test]
fn close_rejects_ignored_content_mutated_between_inventories() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let repository = temporary.path().join("repository");
    let state = temporary.path().join("state");
    fs::create_dir(&repository).expect("create repository");
    git(&repository, &["init", "-b", "main"]);
    git(&repository, &["config", "core.autocrlf", "false"]);
    fs::write(repository.join("tracked.txt"), b"base\n").expect("write tracked file");
    git(&repository, &["add", "."]);
    git(
        &repository,
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

    let manager = CapsuleManager::open(&state).expect("open manager");
    let capsule = manager
        .create(CreateOptions {
            repository,
            base: "HEAD".to_owned(),
            label: Some("ignored close race".to_owned()),
            links: BTreeMap::new(),
        })
        .expect("create capsule");
    fs::write(capsule.workspace_path.join(".gitignore"), b"ignored.log\n")
        .expect("write ignore rule");
    let ignored = capsule.workspace_path.join("ignored.log");
    fs::write(&ignored, b"initial ignored content\n").expect("write ignored content");

    let initial_inventory_captured = Arc::new(Barrier::new(2));
    let mutation_finished = Arc::new(Barrier::new(2));
    *CLOSE_IGNORED_INVENTORY_TEST_HOOK
        .lock()
        .expect("install close test hook") = Some(CloseIgnoredInventoryTestHook {
        capsule_id: capsule.id.clone(),
        initial_inventory_captured: Arc::clone(&initial_inventory_captured),
        mutation_finished: Arc::clone(&mutation_finished),
    });
    let mutation = std::thread::spawn(move || {
        initial_inventory_captured.wait();
        fs::write(ignored, b"mutated ignored content\n").expect("mutate ignored content");
        mutation_finished.wait();
    });

    let close = manager.close(&capsule.id, CloseOptions::default());
    mutation.join().expect("ignored-content mutation thread");
    *CLOSE_IGNORED_INVENTORY_TEST_HOOK
        .lock()
        .expect("clear close test hook") = None;

    assert!(matches!(
        close,
        Err(Error::UnsafeState(message))
            if message.contains("ignored paths or content changed")
                && message.contains("no artifacts were written")
    ));
    assert_eq!(
        manager.show(&capsule.id).expect("active capsule").state,
        crate::model::CapsuleState::Active
    );
    let capsule_state = state.join("capsules").join(&capsule.id);
    assert!(!capsule_state.join("result.patch").exists());
    assert!(!capsule_state.join("result.json").exists());
}

#[test]
fn close_stability_requires_exact_patch_paths_and_head() {
    let original = snapshot(b"patch", &["a"]);
    assert!(
        require_stable_close_snapshot(&original, "head", true, &original, "head", true).is_ok()
    );
    for final_snapshot in [snapshot(b"other", &["a"]), snapshot(b"patch", &["b"])] {
        assert!(matches!(
            require_stable_close_snapshot(
                &original,
                "head",
                true,
                &final_snapshot,
                "head",
                true,
            ),
            Err(Error::UnsafeState(message)) if message.contains("no artifacts were written")
        ));
    }
    assert!(matches!(
        require_stable_close_snapshot(&original, "head", true, &original, "other", true),
        Err(Error::UnsafeState(_))
    ));
    assert!(matches!(
        require_stable_close_snapshot(&original, "head", true, &original, "head", false),
        Err(Error::UnsafeState(_))
    ));
}
