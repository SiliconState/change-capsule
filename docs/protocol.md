# Agent and Automation Protocol

This document is the framework-neutral contract for using Capsule from an agent runner, coding agent, CI job, or orchestration system.

## Contents

- [Core rule](#core-rule)
- [Capability negotiation](#capability-negotiation)
- [Recommended flow](#recommended-flow) — [create](#1-create), [launch](#2-launch-any-worker), [inspect](#3-inspect-during-work), [checkpoint](#4-optional-checkpoint), [evidence](#5-record-evidence), [close](#6-close), [verify](#7-review-verify-or-compare), [integrate](#8-explicit-integration), [cleanup](#9-cleanup)
- [JSON behavior](#json-behavior) — [error kinds](#error-kinds)
- [Links](#links)
- [Concurrency](#concurrency)
- [Recovery](#recovery)

## Core rule

Treat `workspace_path` as the attempt's only writable project directory. Do not switch its branch, move its `.git` marker, or repurpose the path.

## Capability negotiation

```sh
capsule --json capabilities
```

Run this first when you need to know what a given build supports.

It is a static compatibility probe: it executes before `CapsuleManager` or state initialization, never creates, inspects, locks, canonicalizes, or mutates `CAPSULE_HOME`, never invokes Git, and succeeds even if `--home` names a missing, unwritable, malformed, or incompatible state root.

Its output is one bounded deterministic JSON object with no timestamps, host paths, environment-derived values, or nondeterministic ordering. Without `--json` the same document is printed in human-readable form. `Capabilities::current()` is the Rust representation.

```json
{
  "capability_schema_version": 1,
  "product": "change-capsule",
  "product_version": "0.2.0",
  "protocol_versions": [1],
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
    "durable_read_write": [5],
    "receipt_verify": [5],
    "bundle": [1],
    "idempotency_record": [1]
  },
  "limits": {
    "label_bytes": 256,
    "links": 32,
    "link_key_bytes": 64,
    "link_value_bytes": 4096,
    "idempotency_key_bytes": 256
  }
}
```

### Protocol rules

- `capability_schema_version` is independent of the durable capsule schema, receipt schema, bundle schema, and package semver.
- Require a protocol version and the subset of feature identifiers you use. Do not infer behavior from `product_version`.
- Feature identifiers are stable versioned strings. Unknown future identifiers and additive fields are safe to ignore.
- Removing an identifier or changing its meaning requires a new protocol or capability schema version.
- Numeric limits are UTF-8 byte counts unless documented otherwise; `links` is a count.

This document negotiates protocol features. It does not establish trust in the binary, its host, or the state root.

## Recommended flow

### 1. Create

```sh
capsule --json create \
  --repo /path/to/repository \
  --base HEAD \
  --label "implement request" \
  --link task=external-task-id \
  --link run=external-run-id
```

Persist at least:

- `id`;
- `workspace_path`;
- `base_commit`.

The `base` input is resolved to an immutable commit before the attempt starts.

#### Idempotent creation

Any coordinator that can time out, crash, or be restarted should pass a key:

```sh
capsule --json create \
  --repo /path/to/repository \
  --label "implement request" \
  --link task=external-task-id \
  --idempotency-key "run:8f21/attempt:1"
```

An idempotency key is:

- scoped to one canonical Capsule state root — different roots may use the same key independently;
- opaque and compared by exact UTF-8 bytes;
- non-whitespace, control-free, and at most 256 bytes;
- **not a credential**: do not put secrets in it. Prefer high-entropy or namespaced values. Capsule assigns no meaning to key contents;
- durably bound to one logical creation request and one capsule ID for the lifetime of that record, and never reusable after close, integration, orphaning, or drop.

For one state root:

- the same key plus the same creation request always resolves to the same capsule ID;
- concurrent identical calls create at most one capsule identity and worktree;
- retries after a timeout or process crash return or resume that same identity, and a timeout while the original call is still running serializes through the existing locks rather than racing into a second identity;
- the same key with a materially different repository, base request, label, or links fails with `idempotency_conflict` before any second capsule or worktree side effect;
- an unrecoverable or orphaned first attempt stays bound to the key; Capsule marks that same capsule orphaned rather than silently creating a replacement.

**Base selectors.** The reservation stores both the caller's original base selector and the resolved immutable commit. Repeating the same selector — including `HEAD` — replays the original reservation even after the source branch has moved; a different selector is accepted only when its meaning is provably equivalent to the reserved commit, and otherwise conflicts. A key is never silently retargeted to a newer `HEAD`.

The response is the ordinary capsule JSON shape, so no consumer is forced into a conditional response schema. Consumers that need reservation status call lookup.

#### Direct keyed lookup

```sh
capsule --json lookup --idempotency-key "run:8f21/attempt:1"
```

```json
{
  "schema_version": 1,
  "idempotency_key_sha256": "...",
  "capsule_id": "cap-01...",
  "status": "reserved",
  "capsule": null
}
```

Once the manifest exists, `status` is `materialized` and `capsule` carries the validated current manifest.

Lookup is non-mutating and direct: it accesses only the hashed reservation path and its referenced capsule, never `list` or unrelated manifest scans. It fails closed if the reservation and capsule immutable identities disagree, remains usable when unrelated capsule or reservation records are malformed or exceed normal list limits, and returns `idempotency_not_found` without echoing the raw key.

This is the bounded correctness path for crash recovery. Do not use `list` as a recovery primitive on a large multi-agent state root.

**At-most-one identity is not at-most-once execution.** It proves Capsule created one attempt, not that the external agent process ran exactly once. A replay may return a capsule that is already closed, integrated, orphaned, or dropped, so callers still need `status` and targeted `recover <id>`. Idempotency is local orchestration state: it never appears in a portable receipt, and receipts prove result consistency, not agent authorship.

### 2. Launch any worker

Use `workspace_path` as the worker's current directory. Capsule does not care whether the worker is an interactive agent, headless agent, editor, script, or human.

Pseudocode:

```text
capsule = create(...)
worker.cwd = capsule.workspace_path
worker.run(task)
```

Do not assume the worker will commit. Closing captures committed and uncommitted changes.

### 3. Inspect during work

```sh
capsule --json status <id>
capsule --json diff <id> --output /tmp/current.patch
```

Status fields relevant to automation:

- `health`: stop on anything other than `healthy` while active;
- `dirty`: whether the real worktree has uncommitted changes;
- `changed_paths`: complete non-ignored paths changed from the pinned base;
- `ignored_paths`: ignored untracked files/directories deliberately excluded from the patch; their inventory is recorded in the sealed result at close as provenance;
- `commits_ahead`: commits reachable from `HEAD` and not from the base;
- `sealed`: after close, whether the worktree's tracked content still matches the result.

The diff JSON metadata includes `patch_sha256`, a canonical lowercase SHA-256 of the exact live or sealed patch returned; with `--output`, it covers the exact bytes written. Use it rather than a capsule ID alone when deduplicating evidence.

**Close-time stability.** Closing computes a complete ignored-content inventory both before and after its tracked snapshot transaction. The inventories must agree exactly on lossless path identities, byte total, and structural content digest; patch bytes, changed paths, `HEAD`, and the clean/dirty classification used for `result.kind` must also remain identical before any result artifacts are written. The sealed result uses the stable final ignored inventory, and evidence requirements are evaluated against the exact final patch. This narrows ordinary close races but is not an atomic security boundary against hostile same-user mutation after the final checks.

### 4. Optional checkpoint

```sh
capsule --json checkpoint <id> \
  -m "checkpoint description" \
  --author-name "Runner name" \
  --author-email "runner@example.test"
```

Checkpoint constructs the commit through a private index, journals the exact parent, commit, and patch digest, and atomically advances the capsule branch. It does not stage through the worktree's real index. A runner should still not invoke it behind an agent's back unless committing all current workspace content is part of its contract. An interrupted checkpoint reports `health=incomplete_checkpoint` and is handled by `recover` only when its protected Git ref, commit, and journal agree.

### 5. Record evidence

Prefer the executed form. Capsule spawns the program itself in the capsule workspace, with no shell, and records the exit status and a digest of the output it observed:

```sh
capsule --json evidence <id> --timeout-seconds 900 -- cargo test --all-features
```

The resulting record has `executed: true`, and `--require-executed-evidence` on close and verify accepts nothing else. The command runs with no lock held, so a long suite in one capsule does not block others in the same state root. Capsule kills only the process it spawned when the timeout expires.

Record a claim when Capsule genuinely cannot run the command — for example a run on other hardware:

```sh
capsule --json evidence <id> --claim "cargo test on the GPU runner" --exit-code 0
```

A claim is caller-asserted provenance, not an attestation that anything ran, and it can never satisfy an executed-evidence requirement. Either kind binds to the SHA-256 of the complete patch observed when the record is added, so a later edit makes it non-current. A capsule retains at most 64 records totaling at most 256 KiB, and a single command may produce at most 8 MiB of captured output.

### 6. Close

```sh
capsule --json close <id> --require-executed-evidence
```

The three requirements are **independent**, not a ladder:

| Flag | Requires |
| --- | --- |
| `--require-executed-evidence` | one record that Capsule ran itself, that passed, and that is bound to the patch being sealed |
| `--require-current-successful-evidence` | one passing record bound to that patch, executed or merely claimed |
| `--require-successful-evidence` | that **every** record on the capsule passed |

Use `--require-executed-evidence`. It is the only one that checks a fact rather than a caller's assertion, and it implies the second.

The third asks something different and often unwanted: it fails if any earlier record failed, so an attempt whose tests failed once and were then fixed cannot seal. Set it only when a spotless history is genuinely the property you want.

The sealed result is available through:

```sh
capsule --json result <id>
capsule diff <id> --output /tmp/result.patch
capsule --json artifacts <id>
capsule --json export <id> --output /tmp/capsule-result
```

`artifacts` returns a versioned bundle with media types, byte counts, local `file://` URIs, SHA-256 digests, and `sha256:` content addresses. `export` reserves a new no-clobber directory, moves in `result.json` and `result.patch`, and publishes `bundle.json` last as the completion marker.

Rust embedders can call `open_artifact` for a bounded stream or implement `ArtifactSink` to publish to any local, object-store, or CAS backend without coupling core to that backend. Each stream, publication, or export uses one fully read and validated byte snapshot, so a later artifact mutation cannot change bytes already paired with descriptors.

### 7. Review, verify, or compare

Reviewers do not need access to the original worker session. They can consume:

- `result.json` through `capsule result`;
- `result.patch` through `capsule diff`;
- descriptor discovery through `capsule artifacts`;
- a self-describing export through `capsule export`;
- the preserved workspace path while it remains present;
- evidence and external links in the manifest.

An exported bundle is verifiable anywhere, with no capsule state:

```sh
capsule --json verify /tmp/capsule-result --require-current-successful-evidence
capsule --json verify /tmp/capsule-result --repo /path/to/repository
```

Offline verification accepts exported result schema v5 only.

#### Optional signature check

Generate matching raw 32-byte Ed25519 files, then optionally verify a raw 64-byte detached signature with the trusted public key supplied out of band:

```sh
capsule keygen --private-key signing.seed --public-key trusted.pub
capsule --json verify /tmp/capsule-result --signature receipt.sig --trusted-public-key trusted.pub
```

The signature covers a fixed-domain SHA-256 commitment of the exact `bundle.json` bytes; descriptors bind the other two artifacts. Authenticated verification opens `bundle.json` once and applies both checks to that byte snapshot, using only the public key supplied out of band. JSON success sets `signature_authenticated` only after both checks pass.

Key and signature inputs must be regular non-link files of exact raw length; Unix reads open them nonblocking as well as no-follow before post-open validation, so special files cannot wait for a peer. Windows retains reparse-aware opens and checks. Signature output is atomically published without overwrite. Key generation publishes the harmless public key first, so a private-publication failure can leave only that public file for explicit cleanup.

Several capsules may share one task link. A coordinator can compare their changed paths, patches, evidence, and review outcomes before selecting one.

### 8. Explicit integration

```sh
capsule --json integrate <selected-id> --target /path/to/clean/worktree
```

Integration fails if the target moved past the pinned base. The caller must make that decision explicitly: recreate the attempt on a new base, integrate elsewhere, or use its own reviewed conflict-resolution flow.

### 9. Cleanup

```sh
capsule --json drop <id>
```

The durable result remains after drop. `--force` permits cleanup of active or interrupted capsules but never authorizes deletion of a foreign replacement directory.

## JSON behavior

Successful commands emit one JSON value to stdout. Failed commands emit one JSON object to stderr:

```json
{
  "ok": false,
  "error": "human-readable error",
  "kind": "invalid_state"
}
```

### Error kinds

| Kind | Meaning |
| --- | --- |
| `cli` | Argument parsing or CLI usage failure. |
| `not_repository` | The given path is not inside a Git repository. |
| `not_found` | No capsule with that identifier. |
| `invalid_input` | Missing, malformed, or out-of-bounds caller input, including a malformed capsule ID. |
| `invalid_state` | The operation is not allowed from the capsule's current state. |
| `idempotency_conflict` | The key is already bound to a different creation request. |
| `idempotency_not_found` | No reservation exists for that key in this state root. |
| `artifact_not_found` | The requested artifact is not part of this result. |
| `verification` | Receipt verification failed. |
| `safety` | State or worktree ownership could not be proven; fail-closed. |
| `unsealed_result` | The result is unsealed or has drifted since close. |
| `dirty_target` | The integration target is not clean. |
| `git` | A Git invocation failed or produced oversized output. |
| `unsupported_path` | A path cannot be represented in the UTF-8 result inventory. |
| `schema_version` | On-disk or receipt schema is not supported by this build. |
| `internal` | I/O, JSON, or output-capture failure. |

Consumers should branch on `kind` and retain the message for diagnosis. The on-disk manifest and result format is schema version 5. Earlier schemas are not read.

## Links

`--link KEY=VALUE` is intentionally opaque metadata. Keys are bounded identifiers; values are bounded strings. Examples:

```text
task=project-42
run=ci-991
agent=reviewer-2
tracker=example
session=custom-session-id
```

No key has privileged semantics in core.

## Concurrency

**Lock order.** Operations that mutate one repository's capsules take a project-scoped file lock, and also the global lock so cross-repository state cannot race. An executed verification command runs with no lock held, so a long test suite in one capsule never blocks another. Multiple active capsules remain independent Git worktrees.

**Idempotent creation** runs entirely under that same global-then-project lock order. In sequence, it:

1. canonicalizes the requested repository without mutation;
2. resolves or reconciles an existing key reservation;
3. resolves the base for a new key;
4. generates exactly one capsule ID;
5. durably publishes the reservation before any capsule-directory, branch, worktree, or manifest side effect;
6. creates the manifest and worktree using exactly the reserved ID and immutable request;
7. publishes the active manifest as the authoritative completed creation.

The caller should still avoid issuing two mutating commands against the same capsule simultaneously. Locks serialize them, but a stale caller may receive a state error after the first operation completes.

## Recovery

Run at process startup or after an interrupted lifecycle operation:

```sh
capsule --json recover
capsule --json recover <known-id>
```

Global recovery scans all records and fails closed on malformed state. Targeted recovery uses the same transition logic and lock order but reads only the known capsule, so an unrelated malformed record does not block it.

Recovery is conservative. It completes provable journal transitions for create, checkpoint, integration, and cleanup and otherwise leaves state for explicit inspection. Prepared commits are protected by namespaced Git refs until their transition becomes durable. Recovery does not delete work, reset unrelated target changes, or invent a result.

### Creation crash windows

Every window is handled conservatively:

| Interrupted after | Retry behavior |
| --- | --- |
| Nothing yet (before reservation) | May make a new reservation. |
| Reservation published, no capsule directory or manifest | Resumes the reserved ID. |
| Empty reserved capsule directory created | Resumes only when its exact expected private shape can be proven. |
| `creating` manifest published, no worktree | Completes only when the absence of conflicting branch and worktree state is proven. |
| Partial or contradictory Git side effects | Preserves the reservation and marks that same capsule orphaned; never creates a replacement. |
| Capsule already active or later | Replay simply returns it. |

### Submodules and ignored content

Dirty nested submodule worktrees are rejected because a top-level patch cannot contain their internal files. Commit the nested repository first when the desired result is a top-level gitlink change. `status.ignored_paths` reports ignored untracked content that is deliberately excluded from the result.
