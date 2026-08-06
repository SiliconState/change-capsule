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
- discoverable, streamable artifacts with file URIs and SHA-256 content addresses;
- bounded structured lifecycle audit events and aggregate metrics;
- configurable repository, count, age, size, patch, path, and ignored-content policy;
- explicit state inspection, backup, and v2-to-v3 migration;
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

Seal and discover the result. `--require-successful-evidence` rejects missing or failed evidence:

```sh
capsule --json close cap-01... --require-successful-evidence
capsule --json result cap-01...
capsule --json artifacts cap-01...
capsule --json export cap-01... --output ./handoff
```

`artifacts` reports media types, byte lengths, `file://` URIs, and `sha256:` content addresses. `export` reserves a new destination directory without clobbering, moves in `result.json` and `result.patch`, then publishes `bundle.json` last as the completion marker.

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
capsule artifacts    discover sealed artifacts, URIs, sizes, and content addresses
capsule export       create a self-describing result artifact directory
capsule audit        show one capsule's events or the administrative event stream
capsule metrics      show aggregate lifecycle and storage counters
capsule policy       show, replace, or evaluate resource/repository policy
capsule state        inspect, back up, or explicitly migrate durable state
capsule checkpoint   commit current work with an explicit identity
capsule evidence     record externally-run verification evidence
capsule close        seal patch, inventory, evidence, and digest
capsule integrate    explicitly apply one sealed result to its pinned base
capsule drop         safely remove an owned worktree and branch
capsule recover      reconcile interrupted journal states
```

`--json` is global. Errors are emitted as one JSON object on stderr in JSON mode. `capsule diff --json` returns metadata rather than embedding arbitrary patch bytes; pass `--output <file>` for patch data. Policy failures use error kind `policy`; unknown artifact requests use `artifact_not_found`.

State defaults to the platform state directory and can be overridden with `CAPSULE_HOME` or `--home`.

## Policy and operations

An absent `policy.json` means permissive defaults subject to hard safety bounds. Replace policy atomically from a versioned JSON document and evaluate existing state separately:

```sh
capsule --json policy set --file ./capsule-policy.json
capsule --json policy check
capsule --json metrics
capsule --json audit
```

Policy supports allowed repository roots and optional limits for total/live capsule count, capsule age, state/workspace bytes, result patch bytes, changed paths, ignored paths, and ignored bytes. Mutating operations fail before their principal side effect when the applicable limit is exceeded. `policy check` is observational and reports existing violations. For example:

```json
{
  "schema_version": 1,
  "allowed_repository_roots": ["/srv/repositories"],
  "max_capsules": 200,
  "max_live_capsules": 20,
  "max_patch_bytes": 16777216,
  "max_changed_paths": 500,
  "max_ignored_paths": 20,
  "max_ignored_bytes": 104857600,
  "max_capsule_age_seconds": 604800,
  "max_state_bytes": 1073741824,
  "max_workspace_bytes": 10737418240
}
```

State administration is explicit:

```sh
capsule --json state inspect
capsule --json state backup --output ./capsule-backup
capsule --json state migrate --from 2 --backup ./pre-v3-backup
```

Inspection reads schema/version summaries without requiring supported records. Backup and migration require a new destination and copy durable manifests, results, patches, and policy, not live workspaces or Git repositories. Backup publishes `backup.json` last as its completion marker. Migration currently supports only v2 to v3, validates typed v2 identities and result seals before writing, marks the historically unavailable ignored-path inventory incomplete, and always creates a backup first. An interrupted export or backup may leave a reserved destination without its completion marker; callers should treat it as incomplete and choose a new destination or remove it after inspection.

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

The crate owns lifecycle, provenance, artifact descriptors/streams, policy checkpoints, audit records, and state administration. The caller owns process launch, model choice, prompts, credentials, sandboxing, verification execution, and any remote artifact transport.

## Guarantees in the first milestone

1. Multiple attempts may start at the same immutable commit.
2. Each receives a separate ordinary Git worktree and branch.
3. Attempts may change the same files independently.
4. The source worktree remains untouched until explicit integration.
5. Every result has a complete patch, changed-path inventory, digest, sealed provenance, and discoverable artifact descriptors/streams.
6. Lifecycle transitions produce bounded structured audit events; aggregate metrics are available without a daemon.
7. Repository and resource policy is enforced at core mutation boundaries and can be evaluated against current state.
8. Results and journaled checkpoint, integration, and cleanup transitions survive process restart and can be inspected or recovered by another process or agent.
9. Missing, replaced, drifted, or unrepresentable workspaces fail closed.
10. Cleanup refuses foreign directories even with `--force`.
11. Integration is explicit and requires a clean target at the exact pinned base.
12. State can be inspected, backed up, and explicitly migrated from v2 to v3 without a runtime-specific service.

## Scope

Change Capsule owns an attempt boundary, not work planning or agent orchestration.

It composes with issue trackers, agent runners, coding agents, workflow engines, and CI rather than replacing them. An external task ID can be attached with `--link`; no tracker is privileged or required.

See:

- [`docs/architecture.md`](docs/architecture.md)
- [`docs/protocol.md`](docs/protocol.md)
- [`docs/security.md`](docs/security.md)
- [`docs/composition.md`](docs/composition.md)

## Status

This release intentionally supports Git repositories only and expects UTF-8 paths in result inventories. Sparse-checkout, `skip-worktree`, and `assume-unchanged` entries are rejected because an absent or hidden file cannot be distinguished safely from a requested deletion. Dirty nested submodule worktrees and unregistered embedded Git repositories are rejected rather than silently omitted or converted to accidental gitlinks; commit a registered submodule change first if the top-level gitlink should be captured. Ignored untracked paths are excluded but reported by `status.ignored_paths`. Remote execution, distributed persistence, background jobs, non-Git snapshots, automatic rebasing, merge queues, network services, signed attestations, continuous kernel-enforced quotas, and execution sandboxing remain out of scope.

The on-disk capsule/result schema is version 3. Incompatible state fails closed. Explicit migration currently supports v2 to v3 and requires a new backup directory; other versions remain unsupported.

## License

MIT.
