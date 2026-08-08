# Architecture

How Capsule is put together: what it owns, where its state lives, and how a result is built.

## Contents

- [Product boundary](#product-boundary)
- [Why a capsule is not just a worktree](#why-a-capsule-is-not-just-a-worktree)
- [Components](#components)
- [State layout](#state-layout)
- [Protocol surfaces](#protocol-surfaces)
- [Lifecycle](#lifecycle)
- [Result construction](#result-construction)
- [Verification runner](#verification-runner)
- [Artifact interface](#artifact-interface)
- [Integration](#integration)
- [Library and CLI packaging](#library-and-cli-packaging)
- [Future-compatible seams](#future-compatible-seams)

## Product boundary

Capsule represents one code-change attempt as a durable state machine around an ordinary Git worktree.

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
│ state machine · validation · sealing · execution        │
├────────────────────────┬────────────────────────────────┤
│ StateStore             │ Git adapter                    │
│ atomic JSON writes     │ pinned executable              │
│ artifacts + receipts   │ scrubbed GIT_* environment     │
│ global/project locks   │ hooks/signing/ext diff off     │
├────────────────────────┴────────────────────────────────┤
│ Native filesystem and system Git                        │
└─────────────────────────────────────────────────────────┘
```

## State layout

Default roots follow `CAPSULE_HOME`, then `XDG_STATE_HOME`/`HOME` on Unix-like platforms or `LOCALAPPDATA`/`HOME` on Windows.

```text
change-capsule/
├── capsules/
│   └── cap-<ulid>/
│       ├── capsule.json         # lifecycle manifest
│       ├── result.json          # after close
│       └── result.patch         # after close
├── idempotency/
│   └── <domain-separated-sha256-of-key>.json
├── workspaces/
│   └── <project-key>/
│       └── cap-<ulid>/       # Git worktree
└── locks/
    ├── global.lock
    └── project-<key>.lock
```

On Unix, directories are repaired to `0700` and state files to `0600`. Bounded state and artifact reads open with `O_NOFOLLOW | O_NONBLOCK`, then reject non-regular or oversized opened descriptors, so symlinks and special files cannot redirect or wedge a read. Windows uses reparse-point-aware opens and rejects reparse-point descriptors after opening. Writes use a temporary file, `fsync`, atomic persistence, and parent-directory sync. Artifact exports are assembled in a temporary sibling directory, reserve a new destination without clobbering, and publish `bundle.json` last as a completion marker.

The project key is a truncated SHA-256 of the canonical Git common-directory path. It avoids exposing repository names in state paths while grouping locks and workspaces by repository identity.

## Protocol surfaces

Two surfaces exist for orchestrators rather than for a single attempt.

`Capabilities` is a static compatibility document with its own schema version, deliberately independent of the durable capsule schema, the receipt schema, the bundle schema, and package semver. It is a pure function of the build: no timestamps, host paths, environment values, or nondeterministic ordering. The CLI answers `capsule capabilities` before `CapsuleManager` or `StateStore` initialization, so it never creates, inspects, locks, canonicalizes, or mutates `CAPSULE_HOME` and never invokes Git. That is what lets a coordinator probe an unknown or broken installation safely. It negotiates protocol features only; it is not an authenticity or trust claim about the binary.

The `idempotency/` index is a direct keyed reservation store, not a query engine, database, or background index. A reservation is addressed by a domain-separated SHA-256 of the caller's key — the raw key is never a filename — and it carries its own schema version, initially 1, so this index can evolve without bumping the durable capsule/result schema. Each record binds the reserved capsule ID, canonical source worktree and repository common directory, project key, original base selector, resolved immutable base commit, label, links, reservation timestamp, a domain-separated canonical request digest, and a record digest. The request digest is built from explicit length-delimited fields under a versioned domain with deterministic link ordering, and deliberately excludes the reservation timestamp and generated capsule ID so it means request equivalence and nothing else. Reads validate that the filename, stored key digest, request digest, and record digest all agree, so a substituted or rewritten record fails closed rather than being rebuilt.

Idempotent creation publishes the reservation before the first capsule-directory, branch, worktree, or manifest side effect, under the ordinary global-then-project lock order. Every later crash window is resumed only from proof: an empty reserved capsule directory must have exactly the expected private shape, and a `creating` manifest is completed only when the source repository identity still agrees and no conflicting branch or registered worktree exists. Anything ambiguous marks that same capsule orphaned; a replacement identity is never allocated, and no path or branch is deleted to make recovery succeed. Direct lookup reads only the hashed reservation path and the capsule it names, which is why a state root full of unrelated malformed records still resolves one key.

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

Every lifecycle transition is recorded in the capsule manifest as state: the current `state`, the journals for interrupted transitions, checkpoints, evidence, and the result seal. There is no separate event log, daemon, telemetry exporter, or background collector; a reader that wants history reads the manifest.

## Result construction

The result is always computed against the pinned base, not merely against `HEAD`.

To include committed, staged, unstaged, non-ignored untracked, deleted, renamed, and binary content without mutating the user's index, Capsule:

1. creates a private temporary index;
2. loads the base tree into that index;
3. stages the complete worktree into the temporary index;
4. emits a deterministic `git diff --cached --binary --full-index --no-renames --no-color <base>`;
5. emits a NUL-delimited changed-path inventory;
6. hashes the patch with SHA-256.

The real worktree index is not modified by status, diff, close, or drift checks. Consequences of building the snapshot this way:

- Capsule disables sparse checkout while creating its linked workspace, so a sparse source still yields a complete checkout; enabling sparse checkout inside the managed workspace is rejected.
- Because snapshots rebuild an independent index from the base and filesystem, `skip-worktree` and `assume-unchanged` flags in the workspace index do not hide changes.
- Git-ignored untracked files are intentionally outside the patch unless the repository's ignore rules are changed to include them.
- Dirty nested submodules and unregistered embedded repositories remain rejected.

**Ignored-content provenance.** Close computes a complete ignored-content inventory before and after the tracked snapshot transaction and requires exact agreement on path identities, total bytes, and structural content SHA-256 before publishing. The stable final inventory is recorded in the sealed result as provenance. The hash uses a versioned domain and explicit native-path encoding: Unix pathname bytes (including non-UTF-8 names and symlink targets), Windows UTF-16LE code units, or fail-closed UTF-8 on other platforms.

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
evidence records, each carrying the patch digest it was bound to and whether Capsule executed it
creation and seal timestamps
```

`commit` means the worktree was clean and all changes from base were committed. `patch` means uncommitted state was included. Both carry the same complete patch and can be integrated identically.

## Verification runner

`EvidenceInput::Run` spawns the program directly, with the capsule workspace as
its working directory, no shell, and no inherited standard input. Standard
output and error are drained by two threads, so a chatty command cannot block on
a full pipe, and both are bounded at 8 MiB combined; more than that fails rather
than being silently truncated, because a digest over truncated output would not
describe the run. The digest is domain-separated and length-framed across the
two streams, so output cannot be shifted between them without changing it.

The command runs while **no lock is held**. A test suite can take minutes, and
holding the global lock for that long would serialize every other capsule in the
state root, which is exactly what a harness running attempts in parallel is
trying to avoid. Capsule reads the capsule and validates worktree ownership
before running, then re-acquires both locks, re-reads, and re-validates before
recording. A capsule closed or dropped while the command ran therefore fails to
record, rather than writing into a sealed manifest.

An optional timeout kills the spawned process. It does not kill a process group;
see `docs/security.md`.

## Artifact interface

A sealed result exposes two `ArtifactDescriptor` values for `result.json` and `result.patch`. Each descriptor contains a media type, byte length, SHA-256 digest, `sha256:` content address, and percent-encoded local `file://` URI. Embedders may open either artifact as a bounded `ArtifactReader`, publish streams through the runtime-neutral `ArtifactSink` trait, or export both artifacts into a no-clobber destination whose `bundle.json` completion marker is published last. Each operation reads and validates one immutable in-memory byte snapshot before exposing it, preventing later same-sized filesystem mutation from diverging from its descriptors. Core assigns no cloud, CAS, or runtime-specific transport.

An exported bundle is a portable receipt. `verify_bundle` (CLI: `capsule verify`) re-checks it with no capsule state: descriptor digests and sizes, schema versions, and internal result consistency, plus — given a repository — that the pinned base exists and the sealed patch applies to it, reproducing exactly the sealed bytes and changed paths. Emitters and verifiers never need to share a machine.

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

- artifact import and verification;
- Jujutsu or Sapling backends behind a repository-driver trait;
- optional process-job provenance linked to a capsule;
- explicit rebase-as-new-capsule rather than mutating a sealed attempt.

A daemon, task graph, model runtime, or cloud filesystem would be a separate system, not a hidden expansion of the core.
