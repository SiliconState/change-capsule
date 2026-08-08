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
- a static machine-readable capability contract, plus crash-safe idempotent creation with direct keyed lookup;
- explicit state inspection and backup;
- immutable result digest and drift detection;
- explicit, journaled integration;
- guarded cleanup and crash recovery.

No daemon, VFS, container, shell interpreter, object store, task tracker, model SDK, or agent framework is required.

## Contents

- [Who can use it](#who-can-use-it)
- [Install](#install)
- [Quick start](#quick-start)
- [CLI reference](#cli-reference)
- [Orchestration protocol](#orchestration-protocol) — [capabilities](#capability-negotiation), [idempotent creation](#idempotent-creation)
- [Policy and operations](#policy-and-operations) — [policy](#policy), [state administration](#state-administration), [migration](#state-migration)
- [What a receipt proves](#what-a-receipt-proves)
- [Merge gate](#merge-gate) — [committed-receipt protocol](#committed-receipt-protocol)
- [Rust API](#rust-api)
- [Guarantees](#guarantees)
- [Scope](#scope)
- [Status](#status)
- [Documentation](#documentation)

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
change-capsule = { version = "0.2", default-features = false }
```

## Quick start

### 1. Create attempts

Create two independent attempts from the same pinned `HEAD`:

```sh
capsule --json create --repo . --label "approach A" --link task=issue-42
capsule --json create --repo . --label "approach B" --link task=issue-42
```

Each response includes an ID, path, branch, and resolved base commit. Start any agent or command with `workspace_path` as its current directory.

### 2. Inspect during work

```sh
capsule --json status cap-01...
capsule --json diff cap-01... --output /tmp/attempt.patch
```

Each JSON diff response includes canonical lowercase `patch_sha256` for the exact live or sealed patch returned (and, with `--output`, the exact bytes written), so orchestrators can deduplicate only current evidence.

### 3. Checkpoint (optional)

Checkpoint commits are prepared through a private index, journaled, then atomically advanced onto the capsule branch; `recover` finishes an interrupted transition:

```sh
capsule --json checkpoint cap-01... \
  -m "implement parser" \
  --author-name "Automation" \
  --author-email "automation@example.test"
```

### 4. Record evidence

Record verification performed by the caller. Capsule never runs this command; schema-v4 evidence records the SHA-256 of the complete current patch alongside the caller's claim:

```sh
capsule --json evidence cap-01... \
  --command "cargo test" \
  --exit-code 0 \
  --summary "all tests passed"
```

### 5. Seal and export

`--require-successful-evidence` preserves the legacy policy (some evidence, all exit codes zero). `--require-current-successful-evidence` instead requires at least one successful claim bound to the exact patch being sealed. Close accepts a result only when two complete ignored-content inventories surrounding the tracked snapshot transaction agree exactly on path identities, byte total, and structural content digest; it uses the final inventory for policy and the sealed result and fails before publication on instability:

```sh
capsule --json close cap-01... --require-current-successful-evidence
capsule --json result cap-01...
capsule --json artifacts cap-01...
capsule --json export cap-01... --output ./handoff
```

`artifacts` reports media types, byte lengths, `file://` URIs, and `sha256:` content addresses. Artifact readers, publishers, and exports consume one bounded, validated byte snapshot, so later filesystem mutation cannot change the bytes described by that operation. On Unix, bounded sensitive-file reads use no-follow and nonblocking opens before validating the opened descriptor as a regular file, so a FIFO or other special file cannot wedge a read; Windows retains reparse-point-aware opens and post-open checks. `export` reserves a new destination directory without clobbering, moves in `result.json` and `result.patch`, then publishes `bundle.json` last as the completion marker.

### 6. Verify the receipt

Verify anywhere — on the same machine, in CI, or after copying `./handoff` to a reviewer. Verification needs no capsule state; `--repo` additionally proves the sealed patch applies to the pinned base and reproduces exactly the sealed bytes and changed paths:

```sh
capsule --json verify ./handoff --require-current-successful-evidence
capsule --json verify ./handoff --repo . --require-current-successful-evidence
```

### 7. Sign the receipt (optional)

Optional authenticity signs the exact exported `bundle.json` bytes (through a fixed domain-separated SHA-256 commitment) with a raw Ed25519 keypair. Generate matching keys with the OS CSPRNG; both files are exactly 32 raw bytes (the private file is a seed, not PEM/PKCS#8, and the public file is the compressed Ed25519 public key). Key creation publishes the non-secret public key first, never overwrites existing paths, and uses mode `0600` for the private file on Unix. If private publication then fails, the error reports both exact paths and the harmless public file remains for explicit cleanup. The verifier supplies the trusted public key out of band; no key embedded in a receipt is trusted, and Capsule never stores keys in state:

```sh
capsule keygen --private-key ./ed25519.seed --public-key ./ed25519.pub
capsule sign ./handoff --private-key ./ed25519.seed --output ./handoff.sig
capsule --json verify ./handoff --signature ./handoff.sig --trusted-public-key ./ed25519.pub
```

A successful JSON verification reports `"signature_authenticated": true` only after one exact in-memory `bundle.json` snapshot passes both authentication and ordinary receipt verification; the CLI never reopens it between checks. Ordinary receipt verification reports `false`. This proves that the trusted key signed those exact bundle bytes. The bundle descriptors then bind `result.json` and `result.patch`. It does not prove that evidence claims were honestly executed, identify a human, protect a compromised private key, or make the producer trustworthy.

### 8. Integrate and clean up

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

## CLI reference

```text
capsule capabilities print the static machine-readable protocol contract
capsule create       create an isolated attempt from a resolved base commit
capsule lookup       resolve one idempotency key directly, without scanning state
capsule list         list durable capsule records
capsule show         show the full manifest
capsule path         print the ordinary filesystem workspace path
capsule status       inspect health, changed paths, commits, and seal state
capsule diff         emit the complete current or sealed patch
capsule result       show the sealed handoff manifest
capsule artifacts    discover sealed artifacts, URIs, sizes, and content addresses
capsule export       create a self-describing result artifact directory
capsule keygen       generate matching raw Ed25519 private/public key files
capsule sign         create an optional detached Ed25519 signature over bundle.json
capsule verify       verify an exported receipt offline, optionally against a repository
capsule attest       emit an in-toto Statement for a verified receipt
capsule audit        show one capsule's events or the administrative event stream
capsule metrics      show aggregate lifecycle and storage counters
capsule policy       show, replace, or evaluate resource/repository policy
capsule state        inspect, back up, or explicitly migrate durable state
capsule checkpoint   commit current work with an explicit identity
capsule evidence     record externally-run verification evidence
capsule close        seal patch, inventory, evidence, and digest
capsule integrate    explicitly apply one sealed result to its pinned base
capsule drop         safely remove an owned worktree and branch
capsule recover [ID] reconcile all interrupted journals or only one known capsule
```

`--json` is global. Errors are emitted as one JSON object on stderr in JSON mode. `capsule diff --json` returns metadata rather than embedding arbitrary patch bytes; pass `--output <file>` for patch data. Policy failures use error kind `policy`; unknown artifact requests use `artifact_not_found`; idempotency failures use `idempotency_conflict` and `idempotency_not_found`, so no consumer has to parse error text.

State defaults to the platform state directory and can be overridden with `CAPSULE_HOME` or `--home`. `capsule capabilities` is the one command that reads neither.

## Orchestration protocol

Coordinators, CI and evaluation harnesses, task runners, and multi-agent systems need two things before they can drive Capsule safely: a way to know what this build supports, and a way to retry a creation without ever producing a second attempt.

### Capability negotiation

```sh
capsule --json capabilities
```

This is a static compatibility probe. It runs before any state or manager initialization, never creates, inspects, locks, canonicalizes, or mutates `CAPSULE_HOME`, never invokes Git, and succeeds even when `--home` names a missing, unwritable, malformed, or incompatible state root. Its output is one bounded deterministic JSON object with no timestamps, host paths, or environment-derived values:

```json
{
  "capability_schema_version": 1,
  "product": "change-capsule",
  "product_version": "0.2.0",
  "protocol_versions": [1],
  "features": ["cli.structured-errors.v1", "create.idempotent.v1", "..."],
  "schemas": { "durable_read_write": [4], "idempotency_record": [1] },
  "limits": { "label_bytes": 256, "idempotency_key_bytes": 256 }
}
```

Require a protocol version and the subset of feature identifiers you actually use; do not infer behavior from the package version. Feature identifiers are stable versioned strings, unknown identifiers and additive fields are safe to ignore, and removing or redefining an existing identifier requires a new protocol or capability schema version. Every `*_bytes` limit is a UTF-8 byte count. `Capabilities::current()` is the Rust equivalent.

Capabilities negotiate protocol compatibility only. They say nothing about whether the binary is authentic, the host is trustworthy, or the state root is usable.

### Idempotent creation

Retrying a create after a timeout or crash must not produce a second attempt, and discovering whether the first one succeeded must not require scanning a large multi-agent state root:

```sh
capsule --json create --repo . --label "approach A" --idempotency-key "run:8f21/attempt:1"
capsule --json lookup --idempotency-key "run:8f21/attempt:1"
```

An idempotency key is scoped to one canonical state root, compared by exact UTF-8 bytes, and durably bound to one logical creation request and one capsule ID for the lifetime of that record. The same key and the same request always resolve to the same capsule ID; concurrent identical calls create at most one identity and worktree; the same key with a materially different repository, base, label, or links fails with error kind `idempotency_conflict` before any second capsule or worktree side effect. Repeating a selector — including `HEAD` — replays the original reservation even after the source branch has moved, so a key is never silently retargeted to a newer commit. Different state roots may use the same key independently.

Keys are opaque local orchestration state, **not credentials**. Do not put secrets in them: use high-entropy or namespaced values instead. Capsule assigns no meaning to their contents, never stores the raw key as a filename, indexes reservations under a domain-separated SHA-256 digest, and never places idempotency state in a portable receipt — receipts prove result consistency, not who or what produced them.

`create` without `--idempotency-key` keeps its existing behavior and JSON response, and idempotent create returns the ordinary capsule JSON shape, so no consumer is forced into a conditional response schema. `lookup` is non-mutating, reads only the hashed reservation path and the capsule it references, and reports `reserved` before the manifest exists or `materialized` with the validated manifest afterwards. It stays usable when unrelated capsule or reservation records are malformed, and returns error kind `idempotency_not_found` without echoing the raw key.

At-most-one identity is not at-most-once execution: it guarantees Capsule created one attempt, not that an external agent process ran exactly once. A replay can legitimately return a capsule that is already closed, integrated, orphaned, or dropped, so callers still need `capsule status`, targeted `capsule recover <id>`, and `capsule state inspect`. What this does remove is the full-state discovery scan from the normal crash-safe creation path.

## Policy and operations

### Policy

An absent `policy.json` means permissive defaults subject to hard safety bounds. Replace policy atomically from a versioned JSON document and evaluate existing state separately:

```sh
capsule --json policy set --file ./capsule-policy.json
capsule --json policy check
capsule --json metrics
capsule --json audit
```

Policy supports allowed repository roots and optional limits for total/live capsule count, capsule age, state/workspace bytes, result patch bytes, changed paths, ignored paths, and ignored bytes. Result limits apply to the complete base-to-current result, including when a checkpoint contains only a smaller incremental change. Mutating operations fail before their principal side effect when the applicable limit is exceeded. Usage that no configured policy limit references is not measured, except that close always inventories ignored content twice to establish stable sealed provenance. Thus permissive defaults avoid policy-only state/workspace accounting but close still performs its required ignored-content inspection. `policy check` is observational: it evaluates active and sealed results and reports uninspectable workspaces or artifacts as violations rather than silently treating them as compliant. For example:

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

### State administration

State administration is explicit:

```sh
capsule --json state inspect
capsule --json state backup --output ./capsule-backup
```

Inspection reads schema/version summaries without requiring supported records, and separately reports the idempotency index: its record count and any malformed entries, identified by indexed digest rather than raw key. Backup requires a new destination and copies durable manifests, results, patches, policy, and the idempotency index in its indexed layout, not live workspaces or Git repositories. Backup publishes `backup.json` last as its completion marker. An interrupted export or backup may leave a reserved destination without its completion marker; callers should treat it as incomplete and choose a new destination or remove it after inspection.

### State migration

Durable state is schema v4. Exported schema-v3 receipts remain verifiable. Existing schema-v3 state must be migrated explicitly; opening it otherwise fails closed. Dry-run validates all candidate manifests/results without writing or reporting a backup, and a backup argument is rejected unless apply is requested. Apply requires a new external backup directory, publishes its `backup.json` first, then uses an active rollback journal. Migration rejects mixed current/legacy pairs and validates the complete capsule/result seal relationship before backup or writes. After all target writes and syncs, that journal is atomically renamed to a committed-cleanup namespace before deletion, so restart either rolls back active state or only finishes committed cleanup:

```sh
capsule --json state migrate --dry-run
capsule --json state migrate --apply --backup /safe/new/capsule-v3-backup
```

Migrated v3 evidence is explicitly unbound (`patch_sha256` absent), remains a caller claim, and cannot satisfy current-evidence policy.

## What a receipt proves

Stated plainly, because "verifiable AI code" claims more than anything can
deliver. Every attestation carries this same list machine-readably under
`predicate.proof_boundary`.

**Proves**, recomputable by anyone holding the receipt, trusting nothing:

- the patch bytes match their sealed digest and byte count;
- the patch applies to the pinned base and reproduces exactly the sealed bytes
  and changed paths;
- with `--verify-head`, the tree being merged **is** base plus that patch;
- with a signature, a specific out-of-band key signed those exact bundle bytes.

**Does not prove**, and signing does not change this:

- that any recorded evidence command actually ran, or that its output is honest
  — Capsule records claims and never executes them, which is why an attestation
  calls the field `claimed_evidence`;
- who or what wrote the change;
- that the change is correct, safe, reviewed, or good;
- that the producing host was uncompromised.

So a merge gate built on this proves **the diff being merged is the diff that
was reviewed and sealed**. That is an integrity control, not a quality control;
keep running your own tests. If you already use in-toto, SLSA, or Sigstore,
`capsule attest` emits a standard in-toto Statement — see
[`docs/interop.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/interop.md).

## Merge gate

A receipt is only useful if something checks it. This repository ships a GitHub Action that verifies a receipt in CI and, with `verify-head`, refuses the merge unless the tree being merged is exactly the pinned base plus the sealed patch:

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0          # the pinned base must be present
- uses: SiliconState/change-capsule@v0.1.2
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

### Committed-receipt protocol

A receipt cannot describe a commit that contains the receipt itself: adding `bundle.json`, `result.json`, and `result.patch` changes the tree. For repositories that commit receipts, use this exact two-commit protocol:

1. Create the capsule from the branch base and do all implementation work in its workspace.
2. Run verification in that workspace and record the real command and exit code with `capsule evidence`.
3. Close with `--require-successful-evidence`, export the receipt, and integrate the sealed result. This creates the implementation commit.
4. Make one second commit that adds only `receipts/required/bundle.json`, `result.json`, and `result.patch`. Do not amend, squash, or add unrelated files.
5. Push both commits. If the branch is rebased or implementation changes, the old receipt is stale: create a new capsule from the new base and repeat.

The required `receipt-gate` job in this repository enforces the protocol. `scripts/prepare-committed-receipt.sh` rejects a dirty checkout, merge commit, malformed receipt path, missing artifact, or any non-envelope change in the tip commit. It checks out the tip's sole parent and identifies that implementation commit's base. `SiliconState/change-capsule@v0.1.2` then verifies the committed bundle, successful evidence, exact pinned base, and implementation tree byte-for-byte; pull requests additionally require that pinned base to equal GitHub's current base SHA. Configure this job as a required branch check and use rebase or fast-forward merges that preserve the two commits; a squash merge deliberately destroys this binding.

`capsule evidence` records a command claim and exit code supplied by the caller; it does not execute the command or provide signed attestation. The gate proves receipt integrity and tree binding, while CI should still rerun security-critical tests independently.

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

Orchestrators additionally get `Capabilities::current()` for static negotiation, `CapsuleManager::create_idempotent(options, key)` for crash-safe creation, and `CapsuleManager::lookup_idempotency_key(key)` — or `lookup_idempotency_key_at(state_root, key)`, which needs no manager — for direct keyed resolution. `CreateOptions` is unchanged, so existing struct-literal callers keep compiling.

The crate owns lifecycle, provenance, artifact descriptors/streams, policy checkpoints, audit records, and state administration. The caller owns process launch, model choice, prompts, credentials, sandboxing, verification execution, and any remote artifact transport.

## Guarantees

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

## Status

The on-disk capsule/result schema is version 4. Schema-v3 state fails closed until the explicit backup-first migration is applied; exported schema-v3 receipts remain verifiable.

This phase supports Git repositories only. Within that:

- Capsule workspaces disable inherited sparse checkout and materialize a complete checkout even when the source is sparse. Enabling sparse checkout inside the managed workspace remains rejected.
- A private temporary index makes source index `skip-worktree` and `assume-unchanged` flags irrelevant to snapshots.
- Dirty nested submodule worktrees and unregistered embedded Git repositories are rejected rather than silently omitted or converted to accidental gitlinks.
- On Unix, non-UTF-8 inventory paths use `{ "unix_bytes_hex": "..." }`; this form requires lowercase canonical hex and cannot encode valid UTF-8, so every path has one JSON identity.
- Ignored provenance hashes native Unix bytes for names and symlink targets under a versioned domain; Windows uses native UTF-16LE code units under a distinct platform tag.
- Ignored untracked paths are excluded from the patch but recorded as provenance only after two matching close-time inventories.

Out of scope: remote execution, distributed persistence, background jobs, non-Git snapshots, automatic rebasing, merge queues, network services, complex attestation containers, continuous kernel-enforced quotas, and execution sandboxing.

## Documentation

- [`docs/architecture.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/architecture.md) — components, state layout, lifecycle, result construction
- [`docs/protocol.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/protocol.md) — the framework-neutral contract for agents and automation
- [`docs/security.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/security.md) — trust assumptions, protections, and explicit known limits
- [`docs/composition.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/composition.md) — composing with agents, CI, trackers, and multi-agent systems
- [`docs/interop.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/interop.md) — in-toto, SLSA, and Sigstore interoperability, and what a receipt proves
- [`docs/releasing.md`](https://github.com/SiliconState/change-capsule/blob/main/docs/releasing.md) — the coupled receipt-schema and published-pin release protocol

## License

MIT.
