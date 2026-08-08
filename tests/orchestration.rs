//! Capability negotiation and state-root-scoped idempotency protocol tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use change_capsule::{
    Author, Capabilities, CapsuleManager, CapsuleState, CloseOptions, CreateOptions, Error,
    IdempotencyStatus, IntegrateOptions,
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

    fn manager(&self) -> CapsuleManager {
        CapsuleManager::open(&self.state).expect("manager")
    }

    fn options(&self, label: &str) -> CreateOptions {
        let mut options = CreateOptions::new(&self.repo);
        options.label = Some(label.to_owned());
        options.links = BTreeMap::from([("task".to_owned(), "task-1".to_owned())]);
        options
    }

    /// Resolve the indexed reservation path from the public key digest.
    ///
    /// The raw key is never a filename, so tests address the record exactly the
    /// way an operator would: through the digest the lookup response reports.
    fn reservation_path(&self, key: &str) -> PathBuf {
        let digest = self
            .lookup(key)
            .expect("reservation lookup")
            .idempotency_key_sha256;
        self.state
            .join("idempotency")
            .join(format!("{digest}.json"))
    }

    fn lookup(&self, key: &str) -> change_capsule::Result<change_capsule::IdempotencyLookup> {
        CapsuleManager::lookup_idempotency_key_at(&self.state, key)
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.state.join("capsules").join(id).join("capsule.json")
    }
}

#[test]
fn capabilities_current_json_is_an_exact_static_contract() {
    let actual = serde_json::to_string_pretty(&Capabilities::current()).expect("capabilities JSON");
    let expected = r#"{
  "capability_schema_version": 1,
  "product": "change-capsule",
  "product_version": "0.3.0",
  "protocol_versions": [
    1
  ],
  "features": [
    "cli.structured-errors.v1",
    "create.v1",
    "create.idempotent.v1",
    "idempotency.lookup.v1",
    "recover.targeted.v1",
    "diff.sha256.v1",
    "receipt.export.v1",
    "receipt.verify.v1",
    "receipt.attest.intoto.v1",
    "evidence.executed.v1"
  ],
  "schemas": {
    "durable_read_write": [
      5
    ],
    "receipt_verify": [
      5
    ],
    "bundle": [
      1
    ],
    "idempotency_record": [
      1
    ]
  },
  "limits": {
    "label_bytes": 256,
    "links": 32,
    "link_key_bytes": 64,
    "link_value_bytes": 4096,
    "idempotency_key_bytes": 256
  }
}"#;
    assert_eq!(actual, expected);
}

#[test]
fn capabilities_cli_never_touches_unusable_home() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let missing = temporary.path().join("missing").join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "--home"])
        .arg(&missing)
        .arg("capabilities")
        .output()
        .expect("capabilities CLI");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("capabilities value"),
        serde_json::to_value(Capabilities::current()).expect("expected capabilities")
    );
    assert!(!missing.exists());

    let unusable = temporary.path().join("not-a-directory");
    fs::write(&unusable, b"file").expect("unusable home");
    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "--home"])
        .arg(&unusable)
        .arg("capabilities")
        .output()
        .expect("capabilities unusable home");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&unusable).expect("unusable home bytes"), b"file");

    let incompatible = temporary.path().join("incompatible");
    fs::create_dir(&incompatible).expect("incompatible root");
    fs::write(
        incompatible.join("capsule.json"),
        br#"{"schema_version":999}"#,
    )
    .expect("incompatible state marker");
    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "--home"])
        .arg(&incompatible)
        .arg("capabilities")
        .output()
        .expect("capabilities incompatible home");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for created in ["capsules", "workspaces", "locks", "idempotency"] {
        assert!(
            !incompatible.join(created).exists(),
            "{created} was created"
        );
    }

    let human = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--home"])
        .arg(&missing)
        .arg("capabilities")
        .output()
        .expect("human capabilities");
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let rendered = String::from_utf8(human.stdout).expect("human capabilities UTF-8");
    assert!(rendered.contains("change-capsule"), "{rendered}");
    assert!(rendered.contains("create.idempotent.v1"), "{rendered}");
    assert!(!missing.exists());
}

#[test]
fn idempotent_create_replays_one_identity_and_conflicts_before_side_effects() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let first = manager
        .create_idempotent(fixture.options("same"), "run:one")
        .expect("first creation");
    let replay = manager
        .create_idempotent(fixture.options("same"), "run:one")
        .expect("replay");
    assert_eq!(first.id, replay.id);
    assert_eq!(manager.list().expect("list").len(), 1);

    let mut changed = fixture.options("different");
    assert!(matches!(
        manager.create_idempotent(changed.clone(), "run:one"),
        Err(Error::IdempotencyConflict)
    ));
    changed.label = Some("same".to_owned());
    changed.links.insert("other".to_owned(), "value".to_owned());
    assert!(matches!(
        manager.create_idempotent(changed.clone(), "run:one"),
        Err(Error::IdempotencyConflict)
    ));
    changed.links.remove("other");
    changed.base = "HEAD^".to_owned();
    assert!(matches!(
        manager.create_idempotent(changed, "run:one"),
        Err(Error::IdempotencyConflict)
    ));
    let other_repo = fixture.temp.path().join("other-repository");
    let clone = Command::new("git")
        .args(["clone", "--quiet"])
        .arg(&fixture.repo)
        .arg(&other_repo)
        .output()
        .expect("clone other repository");
    assert!(clone.status.success());
    let mut changed_repo = fixture.options("same");
    changed_repo.repository = other_repo;
    assert!(matches!(
        manager.create_idempotent(changed_repo, "run:one"),
        Err(Error::IdempotencyConflict)
    ));
    assert_eq!(manager.list().expect("list after conflicts").len(), 1);
}

#[test]
fn concurrent_identical_creates_materialize_one_capsule_and_worktree() {
    const CONTENDERS: usize = 12;
    let fixture = Fixture::new();
    let barrier = Arc::new(Barrier::new(CONTENDERS));
    let workers: Vec<_> = (0..CONTENDERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let repo = fixture.repo.clone();
            let state = fixture.state.clone();
            std::thread::spawn(move || {
                let manager = CapsuleManager::open(state).expect("contender manager");
                let mut options = CreateOptions::new(repo);
                options.label = Some("concurrent".to_owned());
                barrier.wait();
                manager.create_idempotent(options, "run:concurrent")
            })
        })
        .collect();
    let ids: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("join").expect("create").id)
        .collect();
    assert!(ids.iter().all(|id| id == &ids[0]));
    let manager = fixture.manager();
    assert_eq!(manager.list().expect("list").len(), 1);
    assert_eq!(
        git_text(&fixture.repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        2
    );
}

#[test]
fn state_roots_are_independent_and_head_replay_does_not_retarget() {
    let fixture = Fixture::new();
    let other_state = fixture.temp.path().join("other-state");
    let first = fixture
        .manager()
        .create_idempotent(fixture.options("head"), "shared:key")
        .expect("first root");
    fs::write(fixture.repo.join("later.txt"), b"later\n").expect("later file");
    git(&fixture.repo, &["add", "."]);
    git(
        &fixture.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-m",
            "later",
        ],
    );
    let replay = fixture
        .manager()
        .create_idempotent(fixture.options("head"), "shared:key")
        .expect("HEAD replay");
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.base_commit, first.base_commit);

    let independent = CapsuleManager::open(other_state)
        .expect("other manager")
        .create_idempotent(fixture.options("head"), "shared:key")
        .expect("other root");
    assert_ne!(independent.id, first.id);
}

#[test]
fn lookup_is_direct_and_ignores_unrelated_malformed_state() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = manager
        .create_idempotent(fixture.options("lookup"), "lookup:key")
        .expect("create");
    // Enough unrelated wreckage that any scan-based implementation would either
    // fail closed or become measurably slower than a direct keyed read.
    for index in 0..200 {
        let directory = fixture
            .state
            .join("capsules")
            .join(format!("cap-unrelated-{index}"));
        fs::create_dir(&directory).expect("unrelated capsule dir");
        fs::write(directory.join("capsule.json"), b"not json").expect("bad manifest");
        let digest = format!("{index:064x}");
        fs::write(
            fixture
                .state
                .join("idempotency")
                .join(format!("{digest}.json")),
            b"{}",
        )
        .expect("unrelated reservation");
    }
    fs::write(fixture.state.join("idempotency").join("not-json"), b"bad")
        .expect("bad idempotency filename");

    assert!(
        manager.list().is_err(),
        "unrelated malformed manifests must break a full scan"
    );
    let lookup = manager
        .lookup_idempotency_key("lookup:key")
        .expect("direct lookup");
    assert_eq!(lookup.capsule_id, capsule.id);
    assert_eq!(lookup.status, IdempotencyStatus::Materialized);
    assert_eq!(lookup.capsule.expect("capsule").id, capsule.id);

    // The state-root-only entry point must behave identically without opening a
    // manager, acquiring locks, or discovering Git.
    let direct = fixture.lookup("lookup:key").expect("state-root lookup");
    assert_eq!(direct.capsule_id, capsule.id);
    assert!(matches!(
        fixture.lookup("absent:key"),
        Err(Error::IdempotencyNotFound)
    ));
}

/// A single corrupt manifest must not hide an entire state root.
#[test]
fn skip_invalid_listing_reports_bad_records_without_hiding_good_ones() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let healthy = manager
        .create_idempotent(fixture.options("healthy"), "list:healthy")
        .expect("create");

    let broken = fixture.state.join("capsules").join("cap-corrupted01");
    fs::create_dir(&broken).expect("corrupt record dir");
    fs::write(broken.join("capsule.json"), b"not json").expect("corrupt manifest");

    // The fail-closed path is what callers depend on for exact counts: it must still fail.
    assert!(
        manager.list().is_err(),
        "strict listing must stay fail-closed"
    );

    let listing = manager.list_reporting().expect("lenient listing");
    assert_eq!(listing.capsules.len(), 1);
    assert_eq!(listing.capsules[0].id, healthy.id);
    assert_eq!(listing.unreadable.len(), 1);
    assert_eq!(listing.unreadable[0].id, "cap-corrupted01");
    assert!(!listing.unreadable[0].error.is_empty());

    let output = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "--home"])
        .arg(&fixture.state)
        .args(["list", "--skip-invalid"])
        .output()
        .expect("list --skip-invalid");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("listing JSON");
    assert_eq!(value["capsules"][0]["id"], healthy.id);
    assert_eq!(value["unreadable"][0]["id"], "cap-corrupted01");

    // Without the flag the CLI still refuses to report a partial view.
    let strict = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "--home"])
        .arg(&fixture.state)
        .arg("list")
        .output()
        .expect("strict list");
    assert!(!strict.status.success());
}

#[test]
fn replay_after_every_terminal_lifecycle_state_returns_the_same_identity() {
    let fixture = Fixture::new();
    let manager = fixture.manager();

    // Active.
    let active = manager
        .create_idempotent(fixture.options("active"), "life:active")
        .expect("active create");
    assert_eq!(active.state, CapsuleState::Active);
    assert_eq!(
        manager
            .create_idempotent(fixture.options("active"), "life:active")
            .expect("active replay")
            .id,
        active.id
    );

    // Closed.
    let closed = manager
        .create_idempotent(fixture.options("closed"), "life:closed")
        .expect("closed create");
    fs::write(closed.workspace_path.join("tracked.txt"), b"closed\n").expect("edit closed");
    manager
        .close(&closed.id, CloseOptions::default())
        .expect("close");
    let closed_replay = manager
        .create_idempotent(fixture.options("closed"), "life:closed")
        .expect("closed replay");
    assert_eq!(closed_replay.id, closed.id);
    assert_eq!(closed_replay.state, CapsuleState::Closed);

    // Integrated.
    let integrated = manager
        .create_idempotent(fixture.options("integrated"), "life:integrated")
        .expect("integrated create");
    fs::write(
        integrated.workspace_path.join("tracked.txt"),
        b"integrated\n",
    )
    .expect("edit integrated");
    manager
        .close(&integrated.id, CloseOptions::default())
        .expect("close integrated");
    manager
        .integrate(
            &integrated.id,
            &IntegrateOptions::new(
                fixture.repo.clone(),
                Author::new("Fixture".to_owned(), "fixture@example.test".to_owned()),
            )
            .with_message("integrate".to_owned()),
        )
        .expect("integrate");
    let integrated_replay = manager
        .create_idempotent(fixture.options("integrated"), "life:integrated")
        .expect("integrated replay");
    assert_eq!(integrated_replay.id, integrated.id);
    assert_eq!(integrated_replay.state, CapsuleState::Integrated);

    // Dropped.
    let dropped = manager
        .create_idempotent(fixture.options("dropped"), "life:dropped")
        .expect("dropped create");
    manager
        .drop_capsule(&dropped.id, true)
        .expect("drop capsule");
    let dropped_replay = manager
        .create_idempotent(fixture.options("dropped"), "life:dropped")
        .expect("dropped replay");
    assert_eq!(dropped_replay.id, dropped.id);
    assert_eq!(dropped_replay.state, CapsuleState::Dropped);

    // Orphaned. Written directly here because forcing genuinely contradictory
    // partial Git state is not deterministic from outside the crate; the real
    // orphaning path is covered by the manager's own crash-window unit tests.
    let orphaned = manager
        .create_idempotent(fixture.options("orphaned"), "life:orphaned")
        .expect("orphaned create");
    let manifest_path = fixture.manifest_path(&orphaned.id);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
            .expect("manifest JSON");
    manifest["state"] = serde_json::json!("orphaned");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("orphaned manifest"),
    )
    .expect("write orphaned manifest");
    let orphaned_replay = manager
        .create_idempotent(fixture.options("orphaned"), "life:orphaned")
        .expect("orphaned replay");
    assert_eq!(orphaned_replay.id, orphaned.id);
    assert_eq!(orphaned_replay.state, CapsuleState::Orphaned);

    // Five keys, five identities, no replacements.
    assert_eq!(manager.list().expect("list").len(), 5);
}

#[test]
fn malformed_reservations_fail_closed_promptly() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let capsule = manager
        .create_idempotent(fixture.options("guarded"), "guard:key")
        .expect("create");
    let path = fixture.reservation_path("guard:key");
    let original = fs::read(&path).expect("original reservation");
    let original_manifest_path = fixture.manifest_path(&capsule.id);
    let original_manifest = fs::read(&original_manifest_path).expect("original manifest");

    let as_json =
        |bytes: &[u8]| -> serde_json::Value { serde_json::from_slice(bytes).expect("JSON") };

    // Malformed JSON.
    fs::write(&path, b"{ not json").expect("malformed reservation");
    expect_prompt_failure(&fixture, "guard:key", "malformed JSON");

    // Oversized record.
    let mut oversized = as_json(&original);
    oversized["label"] = serde_json::json!("x".repeat(512 * 1024));
    fs::write(
        &path,
        serde_json::to_vec(&oversized).expect("oversized reservation"),
    )
    .expect("write oversized reservation");
    expect_prompt_failure(&fixture, "guard:key", "oversized record");

    // Filename/digest mismatch: the record claims a key it is not indexed under.
    let mut relabelled = as_json(&original);
    relabelled["idempotency_key_sha256"] = serde_json::json!(format!("{:064x}", 1_u64));
    fs::write(
        &path,
        serde_json::to_vec(&relabelled).expect("relabelled reservation"),
    )
    .expect("write relabelled reservation");
    expect_prompt_failure(&fixture, "guard:key", "filename/digest mismatch");

    // Path substitution: a record for one key placed under another key's digest.
    let other = manager
        .create_idempotent(fixture.options("other"), "other:key")
        .expect("second reservation");
    let other_path = fixture.reservation_path("other:key");
    fs::write(&path, fs::read(&other_path).expect("other reservation"))
        .expect("substitute reservation");
    expect_prompt_failure(&fixture, "guard:key", "path substitution");
    assert_eq!(
        fixture
            .lookup("other:key")
            .expect("other lookup")
            .capsule_id,
        other.id,
        "an unrelated corrupted record must not poison another key"
    );

    // Request mismatch: the stored request digest no longer covers the fields.
    let mut tampered = as_json(&original);
    tampered["base_selector"] = serde_json::json!("main");
    fs::write(
        &path,
        serde_json::to_vec(&tampered).expect("tampered reservation"),
    )
    .expect("write tampered reservation");
    expect_prompt_failure(&fixture, "guard:key", "request mismatch");

    // Capsule-identity mismatch: reservation and manifest disagree.
    fs::write(&path, &original).expect("restore reservation");
    let mut manifest: serde_json::Value = as_json(&original_manifest);
    manifest["label"] = serde_json::json!("retargeted");
    fs::write(
        &original_manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("retargeted manifest"),
    )
    .expect("write retargeted manifest");
    expect_prompt_failure(&fixture, "guard:key", "capsule identity mismatch");
    fs::write(&original_manifest_path, &original_manifest).expect("restore manifest");

    // A restored reservation still resolves, proving the failures were about the
    // corruption and not a poisoned index.
    assert_eq!(
        fixture
            .lookup("guard:key")
            .expect("restored lookup")
            .capsule_id,
        capsule.id
    );

    // A symlinked reservation must not be followed, even to valid content.
    #[cfg(unix)]
    {
        fs::remove_file(&path).expect("remove reservation");
        let elsewhere = fixture.temp.path().join("elsewhere.json");
        fs::write(&elsewhere, &original).expect("symlink target");
        std::os::unix::fs::symlink(&elsewhere, &path).expect("reservation symlink");
        expect_prompt_failure(&fixture, "guard:key", "symlinked reservation");
        fs::remove_file(&path).expect("remove reservation symlink");
        fs::write(&path, &original).expect("restore reservation after symlink");
    }

    // A FIFO must fail on its descriptor's type rather than wait for a writer.
    // Gated to Linux, matching the crate's existing bounded-read FIFO coverage.
    #[cfg(target_os = "linux")]
    {
        fs::remove_file(&path).expect("remove reservation");
        let created = Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("run mkfifo");
        assert!(created.success(), "mkfifo failed: {created}");
        expect_prompt_failure(&fixture, "guard:key", "FIFO reservation");
        fs::remove_file(&path).expect("remove FIFO");
    }
}

/// Assert a corrupted reservation fails closed well inside a wall-clock bound.
///
/// The deadline is what proves a special file cannot wedge the read; the error
/// type is what proves the index is never silently rebuilt.
fn expect_prompt_failure(fixture: &Fixture, key: &str, case: &str) {
    let started = Instant::now();
    let error = fixture
        .lookup(key)
        .expect_err(&format!("{case} must fail closed"));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "{case} took {elapsed:?}, which is not a prompt failure"
    );
    assert!(
        matches!(
            error,
            Error::UnsafeState(_) | Error::Json { .. } | Error::Io { .. }
        ),
        "{case} must fail on the record itself, not read as an absent \
         reservation or an unrelated error: {error:?}"
    );
}

/// The plainest possible reservation: no label, no links.
///
/// Both are omitted from the stored JSON entirely, so this is the case that
/// proves the record still deserializes and revalidates without them.
#[test]
fn minimal_reservation_without_label_or_links_round_trips() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let bare = CreateOptions::new(&fixture.repo);
    assert!(bare.label.is_none());
    assert!(bare.links.is_empty());

    let created = manager
        .create_idempotent(bare, "bare:key")
        .expect("bare creation");
    assert_eq!(created.state, CapsuleState::Active);

    let stored: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.reservation_path("bare:key")).expect("bare reservation"),
    )
    .expect("bare reservation JSON");
    assert!(stored.get("label").is_none(), "{stored}");
    assert!(stored.get("links").is_none(), "{stored}");

    let replay = manager
        .create_idempotent(CreateOptions::new(&fixture.repo), "bare:key")
        .expect("bare replay");
    assert_eq!(replay.id, created.id);

    // Adding a label to the same key is still a materially different request.
    let mut labelled = CreateOptions::new(&fixture.repo);
    labelled.label = Some("now labelled".to_owned());
    assert!(matches!(
        manager.create_idempotent(labelled, "bare:key"),
        Err(Error::IdempotencyConflict)
    ));
    assert_eq!(manager.list().expect("list").len(), 1);
}

#[test]
fn non_idempotent_create_retains_distinct_identity_behavior() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let first = manager.create(fixture.options("plain")).expect("first");
    let second = manager.create(fixture.options("plain")).expect("second");
    assert_ne!(first.id, second.id);
    // Plain create keeps its original contract: a successful return is always a
    // usable active capsule. Only a reserved identity, which can never be
    // replaced, may be handed back orphaned instead.
    assert_eq!(first.state, CapsuleState::Active);
    assert_eq!(second.state, CapsuleState::Active);
    assert!(first.workspace_path.is_dir());
    assert!(second.workspace_path.is_dir());
    assert!(manager.lookup_idempotency_key("plain").is_err());
}

#[test]
fn cli_conflict_and_not_found_have_stable_error_kinds() {
    let fixture = Fixture::new();
    let create = |label: &str| {
        Command::new(env!("CARGO_BIN_EXE_capsule"))
            .args(["--json", "--home"])
            .arg(&fixture.state)
            .args(["create", "--repo"])
            .arg(&fixture.repo)
            .args(["--label", label, "--idempotency-key", "cli:key"])
            .output()
            .expect("create CLI")
    };
    let first = create("one");
    assert!(first.status.success());
    let replay = create("one");
    assert!(replay.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first.stdout).expect("first JSON")["id"],
        serde_json::from_slice::<serde_json::Value>(&replay.stdout).expect("replay JSON")["id"]
    );
    let conflict = create("two");
    assert!(!conflict.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&conflict.stderr).expect("conflict JSON")["kind"],
        "idempotency_conflict"
    );
    let missing = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "--home"])
        .arg(&fixture.state)
        .args(["lookup", "--idempotency-key", "missing:key"])
        .output()
        .expect("missing lookup");
    assert!(!missing.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&missing.stderr).expect("missing JSON")["kind"],
        "idempotency_not_found"
    );

    // The documented success shape of the lookup command itself.
    let found = Command::new(env!("CARGO_BIN_EXE_capsule"))
        .args(["--json", "--home"])
        .arg(&fixture.state)
        .args(["lookup", "--idempotency-key", "cli:key"])
        .output()
        .expect("cli lookup");
    assert!(
        found.status.success(),
        "{}",
        String::from_utf8_lossy(&found.stderr)
    );
    let lookup: serde_json::Value = serde_json::from_slice(&found.stdout).expect("lookup JSON");
    let created: serde_json::Value = serde_json::from_slice(&first.stdout).expect("created JSON");
    assert_eq!(lookup["schema_version"], 1);
    assert_eq!(lookup["status"], "materialized");
    assert_eq!(lookup["capsule_id"], created["id"]);
    assert_eq!(lookup["capsule"]["id"], created["id"]);
    // The response reports the digest, never the caller's raw key.
    let digest = lookup["idempotency_key_sha256"]
        .as_str()
        .expect("key digest");
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    );
    assert!(!found.stdout.windows(7).any(|w| w == b"cli:key"));
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
