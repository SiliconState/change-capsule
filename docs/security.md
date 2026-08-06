# Security Model

Change Capsule protects local attempt identity, result integrity, and cleanup boundaries. It is not a sandbox for untrusted code.

## Trust assumptions

- The user trusts the installed `capsule` binary and system Git executable.
- All callers run as the same local OS user.
- Agent commands may modify anything their own OS process permissions allow.
- The capsule workspace isolates Git state; it does not restrict network, processes, credentials, or paths outside the workspace.

Use an external sandbox when executing untrusted code.

## State protections

State handling is fail-closed:

- state directories must be real non-symlink directories;
- manifest, result, patch, and lock reads reject symlink or non-regular shapes;
- JSON files are capped at 1 MiB;
- patches are capped at 64 MiB;
- Unix state directories are `0700` and files are `0600`;
- writes use an owner-private temporary file, file sync, atomic persistence, and directory sync;
- lock and manifest names derive only from validated capsule IDs and project keys;
- artifact exports and backups require a new non-symlink destination parent, atomically reserve the destination directory, and publish `bundle.json` or `backup.json` last as a completion marker;
- v2-to-v3 migration takes a durable-state backup first and validates typed v2 identities, result metadata, patch bytes, and both stored digests before writing v3 records.

Opaque links and evidence summaries are stored locally but are not secret stores. Do not place credentials in them. Audit events are also local metadata; evidence commands are represented there by SHA-256 but remain present in the evidence record itself.

## Artifact protections

Artifact discovery first revalidates the sealed result. Descriptors report percent-encoded local `file://` URIs and `sha256:` content addresses; these identify bytes but do not grant access or prove authorship. `open_artifact` rechecks regular-file shape and size before returning a bounded local stream. `ArtifactSink` implementations are caller code and inherit the caller's trust, authentication, retention, and transport responsibilities.

Export and backup never overwrite an existing destination and refuse destinations inside managed state. Export copies only a validated sealed result and emits descriptors for the exported paths. Backup copies recognized state files and policy, but intentionally excludes live workspaces, Git object databases, and lock files. A destination lacking its final `bundle.json` or `backup.json` marker is an interrupted, incomplete publication.

## Policy protections and limits

Policy roots are canonicalized before persistence. Global counters are checked while the global lock and applicable project lock are held. Patch/path/ignored-content checks run before checkpoint or close side effects; age, state-byte, workspace-byte, and repository checks run at mutation boundaries.

These are cooperative policy checkpoints, not filesystem reservations or a security sandbox. A worker can consume disk between operations, ignored content can disappear before it is measured, and same-user processes can bypass the crate. Use OS/filesystem quotas and process isolation for continuous hard limits.

## Git process protections

Change Capsule resolves and retains an absolute Git executable path when a manager opens. Each invocation:

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

Change Capsule does not push, pull, fetch, rebase, merge, or run hooks.

## Result integrity

A seal covers:

- capsule ID, label, and opaque links;
- creation and seal timestamps;
- base commit and result HEAD;
- checkpoint records;
- complete binary-capable patch bytes;
- SHA-256 patch digest;
- changed paths;
- ignored-path inventory for native v3 results;
- evidence present at close time.

This detects accidental or same-user mutation after handoff. It does not provide authenticity against a same-user attacker who can rewrite both state and repository content. Signed attestations would be a separate feature.

## Known limits

- Sparse checkout and `skip-worktree` entries are rejected because they can make an absent tracked file indistinguishable from an intended deletion.
- Dirty nested submodule worktrees are rejected because their internal content is not representable by a top-level Git patch; committed gitlink changes remain supported.
- Unregistered embedded Git repositories are rejected rather than silently converted into accidental gitlinks.
- Migration supports only v2 to v3. It creates a backup first, validates old seals, and marks `ignored_paths_complete=false` because v2 did not seal ignored-path inventory; other schema versions fail closed.
- Audit records retain the newest 128 events and report how many older events rolled off; they are validated but neither signed nor append-only against a same-user attacker who can rewrite state.
- Aggregate metrics are instantaneous observations, not monotonic accounting or durable telemetry.
- Policy quotas are checked at lifecycle boundaries rather than continuously enforced.
- Same-user replacement between individual checks remains possible on hostile filesystems; cleanup and integration revalidate Git identities immediately before destructive or target-mutating operations, but the design is not a kernel-enforced capability system.
- UTF-8 paths are required for the changed-path JSON inventory.
- File locks are advisory.
- Evidence is caller-asserted.
- The worktree is not an execution sandbox.
- Windows ACL hardening is not implemented; Unix mode hardening does not translate directly to ACLs.

These limits should remain explicit rather than being obscured behind “agent sandbox” terminology.
