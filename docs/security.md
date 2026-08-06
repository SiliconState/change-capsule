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
- lock and manifest names derive only from validated capsule IDs and project keys.

Opaque links and evidence summaries are stored locally but are not secret stores. Do not place credentials in them.

## Git process protections

Change Capsule resolves and retains an absolute Git executable path when a manager opens. Each invocation:

- clears inherited `GIT_*` variables;
- preserves the ordinary non-Git environment needed to locate platform tools;
- disables hooks;
- disables filesystem monitors;
- disables commit signing;
- disables external diff commands;
- captures stdout and stderr in temporary files with explicit bounds.

Repository content may still contain attributes and configuration interpreted by Git. The first milestone assumes repositories are trusted at the same level as any normal local checkout.

## Cleanup protections

Cleanup is destructive and therefore requires several identities to agree:

- the manifest's canonical Git common directory;
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

The integration transition is journaled before applying the patch. On apply or commit failure, the manager attempts a hard reset to the prior target commit. If rollback cannot be proven, the journal remains `integrating` for explicit diagnosis or conservative recovery.

Change Capsule does not push, pull, fetch, rebase, merge, or run hooks.

## Result integrity

A seal covers:

- base commit;
- result HEAD;
- complete binary-capable patch bytes;
- SHA-256 patch digest;
- changed paths;
- evidence present at close time;
- seal timestamp.

This detects accidental or same-user mutation after handoff. It does not provide authenticity against a same-user attacker who can rewrite both state and repository content. Signed attestations would be a separate feature.

## Known limits

- Same-user replacement between individual checks remains possible on hostile filesystems; cleanup revalidates repository identity immediately before invoking Git, but the design is not a kernel-enforced capability system.
- UTF-8 paths are required for the changed-path JSON inventory.
- File locks are advisory.
- Evidence is caller-asserted.
- The worktree is not an execution sandbox.
- Windows ACL hardening is not implemented; Unix mode hardening does not translate directly to ACLs.

These limits should remain explicit rather than being obscured behind “agent sandbox” terminology.
