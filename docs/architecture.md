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
│ state machine · validation · sealing · integration      │
├────────────────────────┬────────────────────────────────┤
│ StateStore             │ Git adapter                    │
│ atomic JSON            │ pinned executable              │
│ owner-private files    │ scrubbed GIT_* environment     │
│ project locks          │ hooks/signing/ext diff off     │
├────────────────────────┴────────────────────────────────┤
│ Native filesystem and system Git                        │
└─────────────────────────────────────────────────────────┘
```

## State layout

Default roots follow `CAPSULE_HOME`, `XDG_STATE_HOME`, `HOME`, then `LOCALAPPDATA`.

```text
change-capsule/
├── capsules/
│   └── cap-<ulid>/
│       ├── capsule.json
│       ├── result.json       # after close
│       └── result.patch      # after close
├── workspaces/
│   └── <project-key>/
│       └── cap-<ulid>/       # Git worktree
└── locks/
    ├── global.lock
    └── project-<key>.lock
```

On Unix, directories are repaired to `0700` and state files to `0600`. Reads reject symlinked, non-regular, and oversized state files. Writes use a temporary file, `fsync`, atomic persistence, and parent-directory sync.

The project key is a truncated SHA-256 of the canonical Git common-directory path. It avoids exposing repository names in state paths while grouping locks and workspaces by repository identity.

## Lifecycle

```text
              create worktree
 creating ───────────────────▶ active
     │                           │
     │ recover failed            │ close seals patch/result
     ▼                           ▼
 orphaned                     closed
                                 │
                                 │ begin explicit integration
                                 ▼
                             integrating
                              │       │
                   rollback   │       │ commit+journal
                              ▼       ▼
                            closed  integrated
                              │       │
                              └───┬───┘
                                  │ guarded cleanup
                                  ▼
                                dropped
```

`creating` and `integrating` are write-ahead journal states. `recover` reconciles states where the Git side effect and manifest update were interrupted.

## Result construction

The result is always computed against the pinned base, not merely against `HEAD`.

To include committed, staged, unstaged, untracked, deleted, renamed, and binary content without mutating the user's index, Change Capsule:

1. creates a private temporary index;
2. loads the base tree into that index;
3. stages the complete worktree into the temporary index;
4. emits `git diff --cached --binary --full-index <base>`;
5. emits a NUL-delimited changed-path inventory;
6. hashes the patch with SHA-256.

The real worktree index is not modified by status, diff, close, or drift checks.

A closed result is:

```text
schema version
capsule ID
kind: no_change | commit | patch
base and current HEAD commits
complete patch SHA-256 and byte count
changed paths
caller-recorded evidence
seal timestamp
```

`commit` means the worktree was clean and all changes from base were committed. `patch` means uncommitted state was included. Both carry the same complete patch and can be integrated identically.

## Integration

Integration is intentionally conservative:

- the target must belong to the same Git common directory;
- it cannot be the capsule workspace itself;
- it must be clean;
- its `HEAD` must equal the capsule's exact pinned base;
- the capsule workspace must still match its sealed HEAD and patch digest.

The stored patch is applied with Git's indexed three-way apply and committed with an explicit identity. A journal record is written before the side effect. On failure, Change Capsule attempts to reset the target to its pre-integration commit and returns the capsule to `closed` only if rollback succeeds.

No automatic rebase, merge, conflict resolution, or push occurs.

## Library and CLI packaging

`change_capsule` is the reusable crate. The `capsule` binary is a thin adapter over `CapsuleManager`.

The default `cli` feature enables Clap. Consumers embedding only the library may disable default features. The library still invokes system Git; it does not embed or reimplement Git.

## Future-compatible seams

Potential additions that preserve this boundary:

- signed result attestations;
- caller-defined evidence schemas;
- result export/import bundles;
- Jujutsu or Sapling backends behind a repository-driver trait;
- optional process-job provenance linked to a capsule;
- explicit rebase-as-new-capsule rather than mutating a sealed attempt.

A daemon, task graph, model runtime, or cloud filesystem would be a separate system, not a hidden expansion of the core.
