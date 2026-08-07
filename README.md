# Capsule

[![crates.io](https://img.shields.io/crates/v/change-capsule)](https://crates.io/crates/change-capsule)
[![docs.rs](https://img.shields.io/docsrs/change-capsule)](https://docs.rs/change-capsule)
[![CI](https://github.com/SiliconState/change-capsule/actions/workflows/ci.yml/badge.svg)](https://github.com/SiliconState/change-capsule/actions/workflows/ci.yml)

Capsule is an agent-neutral Rust library and CLI for isolated, recoverable code-change attempts — each sealed into a portable, verifiable receipt.

It does not run an agent. It gives any agent or automation system a safe place to work, and it gives everyone downstream — reviewers, CI jobs, merge gates — a durable result whose integrity can be re-checked anywhere with `capsule verify`, without access to the original workspace, session, or state directory.

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
- exported receipts that verify offline, with no capsule state required;
- bounded structured lifecycle audit events and aggregate metrics;
- configurable repository, count, age, size, patch, path, and ignored-content policy;
- explicit state inspection and backup;
- immutable result digest and drift detection;
- explicit, journaled integration;
- guarded cleanup and crash recovery.

No daemon, VFS, container, shell interpreter, object store, task tracker, model SDK, or agent framework is required.

## Who can use it

Any process that can consume a path or JSON can use Capsule: interactive coding agents, headless agents, IDE agents, CI jobs, eval harnesses, task trackers, multi-agent coordinators, and custom scripts.

Capsule does not contain adapters for particular agents. This is intentional. The stable integration surface is:

1. call the CLI with `--json` or embed the Rust crate;
2. give the returned `workspace_path` to the agent as its working directory;
3. let the agent use its existing file, search, shell, and Git tools;
4. record optional evidence;
5. close the capsule and inspect, verify, or integrate its result.

The stable handoff surface is the exported receipt: `bundle.json`, `result.json`, and `result.patch`, content-addressed and verifiable by `capsule verify` on any machine with Git. A harness that emits receipts and a reviewer that checks them never need to share a filesystem, a state directory, or a tool version.

For a complete runnable walkthrough — two competing attempts, sealed receipts, tamper detection, explicit integration — run [`examples/parallel-attempts.sh`](https://github.com/SiliconState/change-capsule/blob/main/examples/parallel-attempts.sh).

## Install

Prerequisites:

- Git on `PATH`;
- Rust 1.85 or newer when building from source.

```sh
cargo install change-capsule
```

That installs the `capsule` binary. The crate is registered as `change-capsule` because `capsule` was already taken on crates.io; the command you run is `capsule`.

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

Verify the receipt anywhere — on the same machine, in CI, or after copying `./handoff` to a reviewer. Verification needs no capsule state; `--repo` additionally proves the sealed patch applies to the pinned base and reproduces exactly the sealed bytes and changed paths:

```sh
capsule --json verify ./handoff --require-successful-evidence
capsule --json verify ./handoff --repo . --require-successful-evidence
```

`artifacts` reports media types, byte lengths, `file://` URIs, and `sha256:` content addresses. Artifact readers, publishers, and exports consume one bounded, validated byte snapshot, so later filesystem mutation cannot change the bytes described by that operation. `export` reserves a new destination directory without clobbering, moves in `result.json` and `result.patch`, then publishes `bundle.json` last as the completion marker.

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
capsule verify       verify an exported receipt offline, optionally against a repository
capsule audit        show one capsule's events or the administrative event stream
capsule metrics      show aggregate lifecycle and storage counters
capsule policy       show, replace, or evaluate resource/repository policy
capsule state        inspect or back up durable state
capsule checkpoint   commit current work with an explicit identity
capsule evidence     record externally-run verification evidence
capsule close        seal patch, inventory, evidence, and digest
capsule integrate    explicitly apply one sealed result to its pinned base
capsule drop         safely remove an owned worktree and branch
capsule recover      reconcile interrupted journal states
```

## Merge gate

A receipt is only useful if something checks it. This repository ships a GitHub Action that verifies a receipt in CI and, with `verify-head`, refuses the merge unless the tree being merged is exactly the pinned base plus the sealed patch:

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0          # the pinned base must be present
- uses: SiliconState/change-capsule@v0.1.0
  with:
    bundle: ./receipt       # produced by `capsule export`
    repo: .
    require-successful-evidence: "true"
    verify-head: "true"
```

The gate passes only when every one of these holds:

1. the bundle's artifacts match their descriptor digests and byte counts;
2. the sealed result is internally consistent and its schema is supported;
3. the pinned base exists in the checkout and the sealed patch applies to it, reproducing exactly the sealed bytes and changed paths;
4. evidence exists and every recorded exit code is zero;
5. the checked-out tree equals base plus the sealed patch.

Together those mean the diff being merged is the diff that was sealed and verified — established without trusting the machine that produced it. Outputs (`verified`, `capsule-id`, `base-commit`, `patch-sha256`, `changed-paths`) are available to later steps, and the run publishes a job summary table.

The same checks run locally with no Action involved:

```sh
scripts/verify-gate.sh --bundle ./receipt --repo . --require-successful-evidence --verify-head
```

### Mandatory committed-receipt protocol

A receipt cannot describe a commit that contains the receipt itself: adding `bundle.json`, `result.json`, and `result.patch` changes the tree. For repositories that commit receipts, use this exact two-commit protocol:

1. Create the capsule from the branch base and do all implementation work in its workspace.
2. Run verification in that workspace and record the real command and exit code with `capsule evidence`.
3. Close with `--require-successful-evidence`, export the receipt, and integrate the sealed result. This creates the implementation commit.
4. Make one second commit that adds only `receipts/required/bundle.json`, `result.json`, and `result.patch`. Do not amend, squash, or add unrelated files.
5. Push both commits. If the branch is rebased or implementation changes, the old receipt is stale: create a new capsule from the new base and repeat.

The required `receipt-gate` job in this repository enforces the protocol. `scripts/prepare-committed-receipt.sh` rejects a dirty checkout, merge commit, malformed receipt path, missing artifact, or any non-envelope change in the tip commit. It checks out the tip's sole parent and identifies that implementation commit's base. `SiliconState/change-capsule@v0.1.0` then verifies the committed bundle, successful evidence, exact pinned base, and implementation tree byte-for-byte; pull requests additionally require that pinned base to equal GitHub's current base SHA. Configure this job as a required branch check and use rebase or fast-forward merges that preserve the two commits; a squash merge deliberately destroys this binding.

`capsule evidence` records a command claim and exit code supplied by the caller; it does not execute the command or provide signed attestation. The gate proves receipt integrity and tree binding, while CI should still rerun security-critical tests independently.

## CLI details

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

Policy supports allowed repository roots and optional limits for total/live capsule count, capsule age, state/workspace bytes, result patch bytes, changed paths, ignored paths, and ignored bytes. Result limits apply to the complete base-to-current result, including when a checkpoint contains only a smaller incremental change. Mutating operations fail before their principal side effect when the applicable limit is exceeded. Usage that no configured limit references is not measured, so the permissive default policy adds no directory walks or content inspection to lifecycle operations. `policy check` is observational: it evaluates active and sealed results and reports uninspectable workspaces or artifacts as violations rather than silently treating them as compliant. For example:

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
```

Inspection reads schema/version summaries without requiring supported records. Backup requires a new destination and copies durable manifests, results, patches, and policy, not live workspaces or Git repositories. Backup publishes `backup.json` last as its completion marker. An interrupted export or backup may leave a reserved destination without its completion marker; callers should treat it as incomplete and choose a new destination or remove it after inspection.

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
5. Every result has a complete patch, changed-path inventory, digest, sealed provenance, and discoverable artifact descriptors/streams, and its exported receipt verifies offline on any machine.
6. Lifecycle transitions produce bounded structured audit events; aggregate metrics are available without a daemon.
7. Repository and resource policy is enforced at core mutation boundaries and can be evaluated against current state.
8. Results and journaled checkpoint, integration, and cleanup transitions survive process restart and can be inspected or recovered by another process or agent.
9. Missing, replaced, drifted, or unrepresentable workspaces fail closed.
10. Cleanup refuses foreign directories even with `--force`.
11. Integration is explicit and requires a clean target at the exact pinned base.
12. State can be inspected and backed up without a runtime-specific service; exported receipts can be verified with no state at all.

## Scope

Capsule owns an attempt boundary, not work planning or agent orchestration.

It composes with issue trackers, agent runners, coding agents, workflow engines, and CI rather than replacing them. An external task ID can be attached with `--link`; no tracker is privileged or required.

See:

- [`docs/architecture.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/architecture.md)
- [`docs/protocol.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/protocol.md)
- [`docs/security.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/security.md)
- [`docs/composition.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/composition.md)

## Status

This release intentionally supports Git repositories only and expects UTF-8 paths in result inventories. Sparse-checkout, `skip-worktree`, and `assume-unchanged` entries are rejected because an absent or hidden file cannot be distinguished safely from a requested deletion. Dirty nested submodule worktrees and unregistered embedded Git repositories are rejected rather than silently omitted or converted to accidental gitlinks; commit a registered submodule change first if the top-level gitlink should be captured. Ignored untracked paths are excluded from the patch but reported by `status.ignored_paths` and recorded — with byte count and content digest — in the sealed result as provenance; because ignored content is exactly what the repository declared irrelevant, its later churn (build output, caches, logs) does not invalidate the seal or block integration and cleanup. Remote execution, distributed persistence, background jobs, non-Git snapshots, automatic rebasing, merge queues, network services, signed attestations, continuous kernel-enforced quotas, and execution sandboxing remain out of scope.

The on-disk capsule/result schema is version 3. Incompatible state fails closed but remains inspectable and backupable; there is no schema migration before a first stable release.

## License

MIT.
