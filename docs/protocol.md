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
- `changed_paths`: complete paths changed from the pinned base;
- `commits_ahead`: commits reachable from `HEAD` and not from the base;
- `sealed`: after close, whether the worktree still matches the result.

### 4. Optional checkpoint

```sh
capsule --json checkpoint <id> \
  -m "checkpoint description" \
  --author-name "Runner name" \
  --author-email "runner@example.test"
```

Checkpoint is explicit because it stages all current worktree changes. A runner should not invoke it behind an agent's back unless that behavior is part of its contract.

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
```

### 7. Review or compare

Reviewers do not need access to the original worker session. They can consume:

- `result.json` through `capsule result`;
- `result.patch` through `capsule diff`;
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

- `not_repository`
- `not_found`
- `invalid_input`
- `invalid_state`
- `safety`
- `unsealed_result`
- `dirty_target`
- `git`
- `unsupported_path`
- `schema_version`
- `internal`

Consumers should branch on `kind` and retain the message for diagnosis. The JSON schema is versioned through each manifest's `schema_version`; additive fields may appear within the same major package line.

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

Operations that mutate one repository's capsules take a project-scoped file lock. Multiple repositories do not block one another. Multiple active capsules in one repository remain independent Git worktrees.

The caller should still avoid issuing two mutating commands against the same capsule simultaneously. Locks serialize them, but a stale caller may receive a state error after the first operation completes.

## Recovery

Run at process startup or after an interrupted lifecycle operation:

```sh
capsule --json recover
```

Recovery is conservative. It completes provable journal transitions and otherwise leaves state for explicit inspection. It does not delete work or invent a result.
