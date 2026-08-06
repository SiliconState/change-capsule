# Change Capsule

Change Capsule is an agent-neutral Rust library and CLI for creating isolated, recoverable, and inspectable code-change attempts.

It does not run an agent. It gives any agent or automation system a safe place to work and a durable result to hand back.

```text
pinned Git commit
      │
      ├── capsule A ── ordinary worktree ── sealed patch + provenance
      ├── capsule B ── ordinary worktree ── sealed patch + provenance
      └── primary worktree remains unchanged until explicit integration
```

A capsule is more than a Git worktree. It records the complete boundary of an attempt:

- collision-resistant identity;
- exact repository and base commit;
- isolated ordinary filesystem path;
- optional links to tasks, runs, agents, or external systems;
- checkpoints created during the attempt, including crash-recoverable checkpoint journals;
- caller-recorded verification evidence;
- complete binary-capable patch, changed-path inventory, and sealed provenance;
- immutable result digest and drift detection;
- explicit, journaled integration;
- guarded cleanup and crash recovery.

No daemon, VFS, container, shell interpreter, object store, task tracker, model SDK, or agent framework is required.

## Who can use it

Any process that can consume a path or JSON can use Change Capsule: interactive coding agents, headless agents, IDE agents, CI jobs, eval harnesses, task trackers, multi-agent coordinators, and custom scripts.

Change Capsule does not contain adapters for particular agents. This is intentional. The stable integration surface is:

1. call the CLI with `--json` or embed the Rust crate;
2. give the returned `workspace_path` to the agent as its working directory;
3. let the agent use its existing file, search, shell, and Git tools;
4. record optional evidence;
5. close the capsule and inspect or integrate its result.

## Install

Prerequisites:

- Git on `PATH`;
- Rust 1.85 or newer when building from source.

```sh
cargo install --path .
```

For library-only use without the Clap CLI dependency:

```toml
[dependencies]
change-capsule = { version = "0.1", default-features = false }
```

## Quick start

Create two independent attempts from the same pinned `HEAD`:

```sh
capsule --json create --repo . --label "approach A" --link task=issue-42
capsule --json create --repo . --label "approach B" --link task=issue-42
```

Each response includes an ID, path, branch, and resolved base commit. Start any agent or command with `workspace_path` as its current directory.

Inspect an active attempt:

```sh
capsule --json status cap-01...
capsule diff cap-01... > /tmp/attempt.patch
```

Optionally create a recoverable checkpoint. Checkpoint commits are prepared through a private index, journaled, then atomically advanced onto the capsule branch; `recover` finishes an interrupted transition:

```sh
capsule --json checkpoint cap-01... \
  -m "implement parser" \
  --author-name "Automation" \
  --author-email "automation@example.test"
```

Record verification performed by the caller:

```sh
capsule --json evidence cap-01... \
  --command "cargo test" \
  --exit-code 0 \
  --summary "all tests passed"
```

Seal the result. `--require-successful-evidence` rejects missing or failed evidence:

```sh
capsule --json close cap-01... --require-successful-evidence
capsule --json result cap-01...
```

Integrate only the selected result into a clean worktree that is still at the pinned base:

```sh
capsule --json integrate cap-01... \
  --target . \
  -m "select parser approach A"
```

Cleanup removes only the capsule-owned worktree and branch. Its manifest, patch, result, and evidence remain:

```sh
capsule --json drop cap-01...
```

## CLI

```text
capsule create       create an isolated attempt from a resolved base commit
capsule list         list durable capsule records
capsule show         show the full manifest
capsule path         print the ordinary filesystem workspace path
capsule status       inspect health, changed paths, commits, and seal state
capsule diff         emit the complete current or sealed patch
capsule result       show the sealed handoff manifest
capsule checkpoint   commit current work with an explicit identity
capsule evidence     record externally-run verification evidence
capsule close        seal patch, inventory, evidence, and digest
capsule integrate    explicitly apply one sealed result to its pinned base
capsule drop         safely remove an owned worktree and branch
capsule recover      reconcile interrupted journal states
```

`--json` is global. Errors are emitted as one JSON object on stderr in JSON mode. `capsule diff --json` returns metadata rather than embedding arbitrary patch bytes; pass `--output <file>` for patch data.

State defaults to the platform state directory and can be overridden with `CAPSULE_HOME` or `--home`.

## Rust API

```rust
use change_capsule::{CapsuleManager, CloseOptions, CreateOptions};

let manager = CapsuleManager::open_default()?;
let mut options = CreateOptions::new(".");
options.label = Some("candidate implementation".into());
let capsule = manager.create(options)?;

// Launch any external tool with capsule.workspace_path as its cwd.

let status = manager.status(&capsule.id)?;
let result = manager.close(&capsule.id, CloseOptions::default())?;
# Ok::<(), change_capsule::Error>(())
```

The crate owns lifecycle and provenance. The caller owns process launch, model choice, prompts, credentials, sandboxing, and verification execution.

## Guarantees in the first milestone

1. Multiple attempts may start at the same immutable commit.
2. Each receives a separate ordinary Git worktree and branch.
3. Attempts may change the same files independently.
4. The source worktree remains untouched until explicit integration.
5. Every result has a complete patch, changed-path inventory, digest, and sealed provenance, including label, links, checkpoints, and evidence.
6. Results and journaled checkpoint, integration, and cleanup transitions survive process restart and can be inspected or recovered by another process or agent.
7. Missing, replaced, drifted, or unrepresentable workspaces fail closed.
8. Cleanup refuses foreign directories even with `--force`.
9. Integration is explicit and requires a clean target at the exact pinned base.

## Scope

Change Capsule owns an attempt boundary, not work planning or agent orchestration.

It composes with issue trackers, agent runners, coding agents, workflow engines, and CI rather than replacing them. An external task ID can be attached with `--link`; no tracker is privileged or required.

See:

- [`docs/architecture.md`](docs/architecture.md)
- [`docs/protocol.md`](docs/protocol.md)
- [`docs/security.md`](docs/security.md)
- [`docs/composition.md`](docs/composition.md)

## Status

This is the smallest convincing milestone. It intentionally supports Git repositories only and expects UTF-8 paths in result inventories. Sparse-checkout, `skip-worktree`, and `assume-unchanged` entries are rejected because an absent or hidden file cannot be distinguished safely from a requested deletion. Dirty nested submodule worktrees and unregistered embedded Git repositories are rejected rather than silently omitted or converted to accidental gitlinks; commit a registered submodule change first if the top-level gitlink should be captured. Ignored untracked paths are excluded but reported by `status.ignored_paths`. Remote execution, distributed persistence, background jobs, non-Git snapshots, automatic rebasing, merge queues, network services, signed attestations, quotas, and execution sandboxing remain out of scope.

The on-disk schema is currently version 2. This pre-release build fails closed on incompatible state rather than silently interpreting an older schema.

## License

MIT.
