# Security Model

Capsule protects local attempt identity, result integrity, and cleanup boundaries. It is not a sandbox for untrusted code.

## Trust assumptions

- The user trusts the installed `capsule` binary and system Git executable.
- All callers run as the same local OS user.
- Agent commands may modify anything their own OS process permissions allow.
- The capsule workspace isolates Git state; it does not restrict network, processes, credentials, or paths outside the workspace.

Use an external sandbox when executing untrusted code.

## State protections

State handling is fail-closed:

- state directories must be real non-symlink directories;
- manifest, result, patch, and lock reads reject symlink or non-regular shapes; on Unix their nonblocking no-follow opens reject special files without waiting for a peer;
- JSON files are capped at 1 MiB;
- patches are capped at 64 MiB;
- Unix state directories are `0700` and files are `0600`;
- writes use an owner-private temporary file, file sync, atomic persistence, and directory sync;
- lock and manifest names derive only from validated capsule IDs and project keys;
- artifact exports and backups require a new non-symlink destination parent, atomically reserve the destination directory, and publish `bundle.json` or `backup.json` last as a completion marker;
- idempotency reservations use those same bounded protections. A raw key is never a filename: records are indexed by a domain-separated SHA-256 of the key. A new reservation is published atomically without overwrite, file-synced, and parent-directory-synced. Reads reject symlinks, reparse points, special files, malformed JSON, oversized records, filename/digest disagreement, path substitution, request-digest mismatch, and reservation/capsule identity mismatch, and a malformed or conflicting record is never silently rebuilt or overwritten. A malformed record for one key does not block a direct lookup for another.

Opaque links, evidence summaries, and idempotency keys are stored locally but are not secret stores. Do not place credentials in them. An idempotency key is opaque orchestration state, not a credential; prefer high-entropy or namespaced values, and note that its digest — not the key — appears in state and in lookup responses. Audit events are also local metadata; evidence commands are represented there by SHA-256 but remain present in the evidence record itself.

## Artifact protections

Artifact discovery first revalidates the sealed result. Descriptors report percent-encoded local `file://` URIs and `sha256:` content addresses; these identify bytes but do not grant access or prove authorship. `open_artifact` rechecks regular-file shape and size before returning a bounded local stream. `ArtifactSink` implementations are caller code and inherit the caller's trust, authentication, retention, and transport responsibilities.

Export and backup never overwrite an existing destination and refuse destinations inside managed state. Export copies only a validated sealed result and emits descriptors for the exported paths. Backup copies recognized state files and policy, but intentionally excludes live workspaces, Git object databases, and lock files. A destination lacking its final `bundle.json` or `backup.json` marker is an interrupted, incomplete publication.

Bundle verification (`capsule verify`) is bounded and fail-closed. Optional authenticity uses a detached raw 64-byte Ed25519 signature over a fixed-domain SHA-256 commitment of the exact `bundle.json` bytes. Authenticated verification reads `bundle.json` once and applies signature and ordinary receipt checks to that same byte snapshot; only a caller-supplied out-of-band public key is trusted. `capsule keygen` obtains a raw 32-byte private seed from the OS CSPRNG and derives the matching raw 32-byte public key; it atomically creates new files without overwrite, publishes the harmless public key first, and gives the private file mode `0600` on Unix. If private publication fails, the public file remains and the reported paths permit explicit cleanup. Key reads use one no-follow/reparse-aware opened handle, add a nonblocking open on Unix, and require a post-open regular file with exact length plus EOF. Library callers can use `generate_keypair` or `derive_public_key`. Private seed buffers are zeroized after generation, derivation, or signing, and keys are never stored in Capsule state. Successful CLI JSON reports whether signature authentication and receipt verification both passed.

## Policy protections and limits

Policy roots are canonicalized before persistence. Global counters are checked while the global lock and applicable project lock are held. Patch/path/ignored-content checks run before checkpoint or close side effects; age, state-byte, workspace-byte, and repository checks run at mutation boundaries.

These are cooperative policy checkpoints, not filesystem reservations or a security sandbox. A worker can consume disk between operations, ignored content can disappear before it is measured, and same-user processes can bypass the crate. Use OS/filesystem quotas and process isolation for continuous hard limits.

## Git process protections

Capsule resolves and retains an absolute Git executable path when a manager opens. Each invocation:

- clears inherited `GIT_*` variables;
- preserves the ordinary non-Git environment needed to locate platform tools;
- disables hooks;
- disables filesystem monitors;
- disables commit signing;
- disables external diff commands;
- captures stdout and stderr through concurrently drained pipes with explicit in-memory bounds.

Repository content may still contain attributes and configuration interpreted by Git. The first milestone assumes repositories are trusted at the same level as any normal local checkout.

## Cleanup protections

Cleanup is destructive and therefore requires several identities to agree:

- the manifest's canonical Git common directory;
- the workspace's recorded and current canonical Git administration directory;
- the workspace's current canonical root;
- the registered Git worktree path;
- the expected capsule branch;
- a non-bare linked worktree.

If a capsule path disappears and is replaced with another repository or ordinary directory, `drop --force` still refuses it. Force changes lifecycle policy; it does not bypass ownership validation.

Ordinary cleanup of a closed or integrated capsule also recomputes the complete patch and HEAD and compares them with the seal. Drift must be reviewed or explicitly forced.

## Integration protections

Before integration:

- the result seal is recomputed;
- source and target must share one canonical Git common directory;
- the target must be a different worktree;
- the target must be clean;
- target `HEAD` must equal the exact base commit;
- author identity must be explicit and bounded.

The integration transition records the target worktree and its exact Git administration directory before any target change. A candidate commit is constructed through a private index, checked against the sealed patch, and protected by a namespaced pending ref. The target is revalidated immediately before a fast-forward. Recovery finalizes only that exact commit or restores a provably untouched journal to `closed`; it never hard-resets unrelated target work.

Capsule does not push, pull, fetch, rebase, merge, or run hooks.

## Result integrity

A seal covers:

- capsule ID, label, and opaque links;
- creation and seal timestamps;
- base commit and result HEAD;
- checkpoint records;
- complete binary-capable patch bytes;
- SHA-256 patch digest;
- changed paths;
- the close-time ignored-path inventory (recorded as provenance; later churn of ignored content does not invalidate the seal);
- evidence present at close time.

Evidence records are explicit caller claims. Schema-v4 evidence binds to the complete patch digest observed when attached, but Capsule still does not execute or attest to the command. Detached receipt signing can authenticate the exact exported bytes to an out-of-band trusted key; it does not make evidence truthful.

## Capability and idempotency limits

`capsule capabilities` negotiates protocol compatibility. It is a static statement about what this build implements, not evidence that the binary is authentic, that its host is trustworthy, or that its state root is usable. It deliberately touches neither state nor Git, so it also cannot tell a caller whether an installation is healthy.

Idempotent creation guarantees at most one capsule identity and worktree per key within one state root. It does not guarantee that the external agent process ran exactly once — Capsule does not launch, observe, or supervise that process. A replay may legitimately return a capsule that is already closed, integrated, orphaned, or dropped, so callers still need targeted lifecycle recovery and state inspection rather than treating a successful replay as "the work is in progress". Idempotency is local orchestration state and is deliberately absent from portable receipts: a receipt proves result consistency, not agent authorship or execution count.

## Known limits

- A capsule created from a sparse source materializes a complete independent checkout; enabling sparse checkout inside the managed workspace is rejected. Independent temporary indexes make `skip-worktree` and `assume-unchanged` flags irrelevant to snapshots.
- Dirty nested submodule worktrees are rejected because their internal content is not representable by a top-level Git patch; committed gitlink changes remain supported.
- Unregistered embedded Git repositories are rejected rather than silently converted into accidental gitlinks.
- Schema-v3 durable state requires explicit backup-first migration; exported schema-v3 receipts remain verifiable, and migrated evidence remains unbound.
- Audit records retain the newest 128 events and report how many older events rolled off; they are validated but neither signed nor append-only against a same-user attacker who can rewrite state.
- Aggregate metrics are instantaneous observations, not monotonic accounting or durable telemetry.
- Policy quotas are checked at lifecycle boundaries rather than continuously enforced.
- Same-user replacement between individual checks remains possible on hostile filesystems; close requires matching complete ignored-content inventories around its tracked snapshot transaction, revalidates a second complete tracked snapshot and `HEAD`, and then writes artifacts. Cleanup and integration revalidate Git identities immediately before destructive or target-mutating operations. A hostile same-user mutation after close's final checks remains outside the security boundary: close is not atomic against such an actor, and the design is not a kernel-enforced capability system.
- On Unix, non-UTF-8 Git inventory paths use an unambiguous raw-byte hex JSON representation; ordinary UTF-8 paths remain strings.
- File locks are advisory.
- Evidence is caller-asserted.
- The worktree is not an execution sandbox.
- Windows ACL hardening is not implemented. Portable metadata checks reject symlink/reparse-like managed entries when exposed as non-regular/non-directory entries, but std does not provide robust owner-private ACL enforcement; Capsule therefore does not claim Windows state is owner-private.

These limits should remain explicit rather than being obscured behind “agent sandbox” terminology.
