# Architecture

## Product boundary

Change Capsule represents one code-change attempt as a durable state machine around an ordinary Git worktree.

It deliberately does not represent:

- a task or issue;
- an agent identity or model session;
- a workflow graph;
- a shell or process supervisor;
- a filesystem abstraction;
- a remote execution service;
- a merge queue.

Those systems may create and consume capsules through the CLI, JSON, or Rust crate.

## Why a capsule is not just a worktree

A worktree answers “where are these files?” A capsule also answers:

- Which exact commit did the attempt start from?
- Which external task or run requested it?
- Which commits and verification claims belong to the attempt?
- What complete change should another process review?
- Has the result changed since it was handed off?
- Was this result explicitly integrated, or merely produced?
- Can cleanup prove that the path is still the object it created?

Git remains the source of truth for repository content. Capsule state provides lifecycle and provenance around it.

## Components

```text
┌─────────────────────────────────────────────────────────┐
│ Callers                                                  │
│ agents · scripts · CI · trackers · workflow engines     │
└────────────────────────┬────────────────────────────────┘
                         │ Rust API or CLI/JSON
┌────────────────────────▼────────────────────────────────┐
│ CapsuleManager                                           │
│ state machine · validation · sealing · policy           │
├────────────────────────┬────────────────────────────────┤
│ StateStore             │ Git adapter                    │
│ atomic JSON + backups  │ pinned executable              │
│ artifacts + audit      │ scrubbed GIT_* environment     │
│ global/project locks   │ hooks/signing/ext diff off     │
├────────────────────────┴────────────────────────────────┤
│ Native filesystem and system Git                        │
└─────────────────────────────────────────────────────────┘
```

## State layout

Default roots follow `CAPSULE_HOME`, then `XDG_STATE_HOME`/`HOME` on Unix-like platforms or `LOCALAPPDATA`/`HOME` on Windows.

```text
change-capsule/
├── policy.json                  # optional, versioned policy
├── capsules/
│   └── cap-<ulid>/
│       ├── capsule.json         # lifecycle plus bounded audit events
│       ├── result.json          # after close
│       └── result.patch         # after close
├── workspaces/
│   └── <project-key>/
│       └── cap-<ulid>/       # Git worktree
└── locks/
    ├── global.lock
    └── project-<key>.lock
```

On Unix, directories are repaired to `0700` and state files to `0600`. Reads reject symlinked, non-regular, and oversized state files. Writes use a temporary file, `fsync`, atomic persistence, and parent-directory sync. Artifact exports and backups are assembled in temporary sibling directories, reserve a new destination without clobbering, and publish `bundle.json` or `backup.json` last as a completion marker.

The project key is a truncated SHA-256 of the canonical Git common-directory path. It avoids exposing repository names in state paths while grouping locks and workspaces by repository identity.

## Lifecycle

```text
                    create worktree
 creating ─────────────────────────────▶ active
    │                                      │  checkpoint request
    │ recover cannot prove ownership       ▼
    ▼                                 checkpointing
 orphaned                                  │  journal/ref recovery
                                           ▼
                                         active
                                           │  close seals patch/result
                                           ▼
                                         closed
                                           │  explicit integration
                                           ▼
                                       integrating
                                       │         │
                              no side  │         │ exact commit+journal
                              effect   ▼         ▼
                                     closed   integrated
                                       │         │
                                       └────┬────┘
                                            │ guarded cleanup
                                            ▼
                                         dropping
                                            │ journal recovery
                                            ▼
                                          dropped
```

`creating`, `checkpointing`, `integrating`, and `dropping` are journal states. Prepared checkpoint and integration commits are temporarily protected by namespaced Git refs until they become reachable from the capsule or target branch. `recover` completes only transitions whose worktree identity, Git ref or branch, commit parent, patch, and journal agree; ambiguous state remains available for explicit diagnosis.

Successful lifecycle operations retain the newest 128 versioned `AuditEvent` records in the capsule manifest and increment `audit_events_dropped` when older records roll off. Events identify the capsule/project, transition, timestamp, and bounded attributes; evidence commands are represented by SHA-256 rather than copied into event attributes. `audit_log` merges per-capsule records into an administrative stream while preserving each capsule's order. `metrics` computes aggregate state, artifact-byte, retained/dropped-event, and storage counters on demand. There is no daemon, telemetry exporter, network transmission, or background collector.

## Result construction

The result is always computed against the pinned base, not merely against `HEAD`.

To include committed, staged, unstaged, non-ignored untracked, deleted, renamed, and binary content without mutating the user's index, Change Capsule:

1. creates a private temporary index;
2. loads the base tree into that index;
3. stages the complete worktree into the temporary index;
4. emits a deterministic `git diff --cached --binary --full-index --no-renames --no-color <base>`;
5. emits a NUL-delimited changed-path inventory;
6. hashes the patch with SHA-256.

The real worktree index is not modified by status, diff, close, or drift checks. Git-ignored untracked files are intentionally outside the patch unless the repository's ignore rules are changed to include them. Native v3 results seal their path inventory, total bytes, file/link/directory structure, symlink targets, and content SHA-256 so excluded content cannot change silently after close. Sparse/`skip-worktree` checkouts, dirty nested submodules, and unregistered embedded repositories are rejected rather than being misrepresented as complete snapshots.

A closed result is:

```text
schema version
capsule ID, label, and opaque links
kind: no_change | commit | patch
base and current HEAD commits
complete patch SHA-256 and byte count
changed paths
ignored-path inventory plus excluded-content byte count and SHA-256
checkpoint records
caller-recorded evidence
creation and seal timestamps
```

`commit` means the worktree was clean and all changes from base were committed. `patch` means uncommitted state was included. Both carry the same complete patch and can be integrated identically.

## Artifact interface

A sealed result exposes two `ArtifactDescriptor` values for `result.json` and `result.patch`. Each descriptor contains a media type, byte length, SHA-256 digest, `sha256:` content address, and percent-encoded local `file://` URI. Embedders may open either artifact as a bounded `ArtifactReader`, publish streams through the runtime-neutral `ArtifactSink` trait, or export both artifacts into a no-clobber destination whose `bundle.json` completion marker is published last. Core assigns no cloud, CAS, or runtime-specific transport.

## Policy and quotas

`policy.json` has an independent schema version. An absent file means permissive defaults under the fixed 64 MiB patch safety bound. Policy may allowlist canonical repository roots and limit total/live records, age, observed state/workspace bytes, patch bytes, changed paths, ignored paths, and ignored content bytes. Lifecycle mutations check applicable policy while holding the global and project locks. `policy_report` evaluates existing state without mutating it.

Byte/count quotas are cooperative checkpoints, not kernel reservations: workers can grow workspaces between capsule operations, and another same-user process can consume disk independently. Every relevant core mutation rechecks observed usage; callers that need continuous hard enforcement must add filesystem or OS quotas.

## State administration

`inspect_state` reports record schema/state summaries without deserializing records as the current schema. `backup_state` copies recognized durable manifests, results, patches, and policy under all known project locks; workspaces and Git repositories are deliberately excluded, and `backup.json` is the completion marker. `migrate_state` currently supports only v2 to v3, requires a new backup destination, validates typed v2 manifests and sealed result/patch digests before writing, and marks migrated ignored-path inventories incomplete because v2 did not seal that field. Migration is restartable across per-file atomic writes. A reserved export/backup directory without its marker is incomplete and is never reused implicitly.

## Integration

Integration is intentionally conservative:

- the target must belong to the same Git common directory;
- it cannot be the capsule workspace itself;
- it must be clean;
- its `HEAD` must equal the capsule's exact pinned base;
- the capsule workspace must still match its sealed HEAD and patch digest.

The stored patch is applied to a private index and materialized as a candidate commit with an explicit identity. The candidate is checked for one exact parent and byte-for-byte reproduction of the sealed patch, protected by a namespaced pending ref, and only then fast-forwarded onto the target. The target Git directory, cleanliness, and `HEAD` are revalidated immediately before that side effect. Recovery finalizes only the exact journaled commit or restores a provably untouched target to `closed`; it never resets unrelated target work.

No automatic rebase, merge, conflict resolution, or push occurs.

## Library and CLI packaging

`change_capsule` is the reusable crate. The `capsule` binary is a thin adapter over `CapsuleManager`.

The default `cli` feature enables Clap. Consumers embedding only the library may disable default features. The library still invokes system Git; it does not embed or reimplement Git.

## Future-compatible seams

Potential additions that preserve this boundary:

- signed result attestations;
- caller-defined evidence schemas;
- artifact import and verification;
- Jujutsu or Sapling backends behind a repository-driver trait;
- optional process-job provenance linked to a capsule;
- explicit rebase-as-new-capsule rather than mutating a sealed attempt.

A daemon, task graph, model runtime, or cloud filesystem would be a separate system, not a hidden expansion of the core.
