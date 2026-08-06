# Agent and Automation Protocol

This document is the framework-neutral contract for using Change Capsule from an agent runner, coding agent, CI job, or orchestration system.

## Core rule

Treat `workspace_path` as the attempt's only writable project directory. Do not switch its branch, move its `.git` marker, or repurpose the path.

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

### 2. Launch any worker

Use `workspace_path` as the worker's current directory. Change Capsule does not care whether the worker is an interactive agent, headless agent, editor, script, or human.

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
- `ignored_paths`: ignored untracked files/directories deliberately excluded from the patch but covered by the native v3 excluded-content seal;
- `commits_ahead`: commits reachable from `HEAD` and not from the base;
- `sealed`: after close, whether the worktree still matches the result.

### 4. Optional checkpoint

```sh
capsule --json checkpoint <id> \
  -m "checkpoint description" \
  --author-name "Runner name" \
  --author-email "runner@example.test"
```

Checkpoint constructs the commit through a private index, journals the exact parent, commit, and patch digest, and atomically advances the capsule branch. It does not stage through the worktree's real index. A runner should still not invoke it behind an agent's back unless committing all current workspace content is part of its contract. An interrupted checkpoint reports `health=incomplete_checkpoint` and is handled by `recover` only when its protected Git ref, commit, and journal agree.

### 5. Record evidence

Change Capsule records evidence; it does not execute verification commands.

```sh
capsule --json evidence <id> \
  --command "cargo test --all-features" \
  --exit-code 0 \
  --summary "5 integration tests passed"
```

The caller is responsible for command execution, timeout, sandboxing, output retention, and honesty. Evidence is provenance, not a cryptographic attestation.

### 6. Close

```sh
capsule --json close <id> --require-successful-evidence
```

After close, treat the worktree as read-only. Any subsequent mutation causes `status.health=drifted_after_close`, blocks ordinary cleanup, and blocks integration.

The sealed result is available through:

```sh
capsule --json result <id>
capsule diff <id> --output /tmp/result.patch
capsule --json artifacts <id>
capsule --json export <id> --output /tmp/capsule-result
```

`artifacts` returns a versioned bundle with media types, byte counts, local `file://` URIs, SHA-256 digests, and `sha256:` content addresses. `export` reserves a new no-clobber directory, moves in `result.json` and `result.patch`, and publishes `bundle.json` last as the completion marker. Rust embedders can call `open_artifact` for a bounded stream or implement `ArtifactSink` to publish to any local, object-store, or CAS backend without coupling core to that backend. Each stream, publication, or export uses one fully read and validated byte snapshot, so a later artifact mutation cannot change bytes already paired with descriptors.

### 7. Review or compare

Reviewers do not need access to the original worker session. They can consume:

- `result.json` through `capsule result`;
- `result.patch` through `capsule diff`;
- descriptor discovery through `capsule artifacts`;
- a self-describing export through `capsule export`;
- the preserved workspace path while it remains present;
- evidence and external links in the manifest.

Several capsules may share one task link. A coordinator can compare their changed paths, patches, evidence, and review outcomes before selecting one.

### 8. Explicit integration

```sh
capsule --json integrate <selected-id> --target /path/to/clean/worktree
```

Integration fails if the target moved past the pinned base. The caller must make that policy decision explicitly: recreate the attempt on a new base, integrate elsewhere, or use its own reviewed conflict-resolution flow.

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

Current error kinds are:

- `cli`
- `not_repository`
- `not_found`
- `invalid_input`
- `invalid_state`
- `policy`
- `artifact_not_found`
- `safety`
- `unsealed_result`
- `dirty_target`
- `git`
- `unsupported_path`
- `schema_version`
- `internal`

Consumers should branch on `kind` and retain the message for diagnosis. `cli` covers argument-parser failures; JSON help and version requests instead succeed with `kind=cli_help` and an `output` field. All other listed error kinds come from library operations. The on-disk manifest and result format is schema version 3. Incompatible schemas fail closed; additive JSON output fields may appear within the same major package line.

## Audit and metrics

Every successful lifecycle mutation retains a versioned event in the capsule manifest, up to the newest 128 events. When older events roll off, `audit_events_dropped` increments and aggregate metrics report the dropped count. Read one stream with `capsule --json audit <id>` or all retained records with `capsule --json audit`. Events include transition identities and bounded attributes; evidence commands are represented by digest rather than duplicated verbatim. They are an administrative history, not a cryptographically signed or append-only ledger.

`capsule --json metrics` computes an instantaneous snapshot of capsule states, live/sealed counts, result patch bytes, state/workspace bytes, and retained/dropped event counts. Core does not run a collector or transmit telemetry. Callers may scrape this command or use `CapsuleManager::metrics`.

## Policy

An absent policy means permissive defaults under hard safety bounds. `capsule policy set --file <json>` atomically replaces versioned policy; `capsule policy check` evaluates current records. Policy may allowlist canonical repository roots and limit total/live records, age, state/workspace bytes, patch bytes, changed paths, ignored paths, and ignored content bytes. Patch and changed-path limits apply to the complete base-to-current result at close and checkpoint boundaries, not only the newest checkpoint delta. Policy checking evaluates active and sealed results and reports unavailable or invalid usage as a violation.

Policy is checked at core lifecycle boundaries. It is not a continuous filesystem reservation: a worker may grow a workspace between commands. Use OS/filesystem quotas when hard continuous enforcement is required.

## State administration

`capsule state inspect` reports record versions and states even when normal schema deserialization would fail. `capsule state backup --output <new-directory>` copies recognized durable state and policy, excluding workspaces and Git repositories, and publishes `backup.json` last as the completion marker. `capsule state migrate --from 2 --backup <new-directory>` is the only supported migration; it always backs up first, validates typed v2 identities and result seals, and marks the unavailable v2 ignored-path inventory incomplete. Migration uses atomic per-file writes and is safe to retry after interruption. `bundle.json` similarly marks a complete artifact export; a reserved destination lacking its marker is incomplete and is not reused implicitly.

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

Operations that mutate one repository's capsules take a project-scoped file lock. Policy-sensitive mutations also take the global lock so global counters cannot race. Backup, migration, policy replacement/checking, metrics, inspection, and the administrative audit stream take the global lock plus every known project lock in deterministic order. Multiple active capsules remain independent Git worktrees.

The caller should still avoid issuing two mutating commands against the same capsule simultaneously. Locks serialize them, but a stale caller may receive a state error after the first operation completes.

## Recovery

Run at process startup or after an interrupted lifecycle operation:

```sh
capsule --json recover
```

Recovery is conservative. It completes provable journal transitions for create, checkpoint, integration, and cleanup and otherwise leaves state for explicit inspection. Prepared commits are protected by namespaced Git refs until their transition becomes durable. Recovery does not delete work, reset unrelated target changes, or invent a result.

Dirty nested submodule worktrees are rejected because a top-level patch cannot contain their internal files. Commit the nested repository first when the desired result is a top-level gitlink change. `status.ignored_paths` reports ignored untracked content that is deliberately excluded from the result.
