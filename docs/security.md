# Security Model

Capsule protects local attempt identity, result integrity, and cleanup boundaries. It is not a sandbox for untrusted code.

## Contents

- [Trust assumptions](#trust-assumptions)
- [State protections](#state-protections)
- [Artifact protections](#artifact-protections) — [receipt signing](#receipt-signing)
- [Git process protections](#git-process-protections)
- [Cleanup protections](#cleanup-protections)
- [Integration protections](#integration-protections)
- [Result integrity](#result-integrity)
- [Capability and idempotency limits](#capability-and-idempotency-limits)
- [Scaling characteristics](#scaling-characteristics)
- [Known limits](#known-limits)

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
- artifact exports require a new non-symlink destination parent, atomically reserve the destination directory, and publish `bundle.json` last as a completion marker;
- idempotency reservations use those same bounded protections. A raw key is never a filename: records are indexed by a domain-separated SHA-256 of the key. A new reservation is published atomically without overwrite, file-synced, and parent-directory-synced. Reads reject symlinks, reparse points, special files, malformed JSON, oversized records, filename/digest disagreement, path substitution, request-digest mismatch, and reservation/capsule identity mismatch, and a malformed or conflicting record is never silently rebuilt or overwritten. A malformed record for one key does not block a direct lookup for another.

Opaque links, evidence summaries, and idempotency keys are stored locally but are not secret stores. Do not place credentials in them. An idempotency key is opaque orchestration state, not a credential; prefer high-entropy or namespaced values, and note that its digest — not the key — appears in state and in lookup responses. An executed record stores the command line and a bounded tail of its output, so a command that prints a secret puts that secret in the receipt.

## Artifact protections

Artifact discovery first revalidates the sealed result. Descriptors report percent-encoded local `file://` URIs and `sha256:` content addresses; these identify bytes but do not grant access or prove authorship. `open_artifact` rechecks regular-file shape and size before returning a bounded local stream. `ArtifactSink` implementations are caller code and inherit the caller's trust, authentication, retention, and transport responsibilities.

Export never overwrites an existing destination and refuses destinations inside managed state. It copies only a validated sealed result and emits descriptors for the exported paths. A destination lacking its final `bundle.json` marker is an interrupted, incomplete publication.

### Receipt signing

Bundle verification (`capsule verify`) is bounded and fail-closed. Optional authenticity uses a detached raw 64-byte Ed25519 signature over a fixed-domain SHA-256 commitment of the exact `bundle.json` bytes.

- **Verification** reads `bundle.json` once and applies signature and ordinary receipt checks to that same byte snapshot; only a caller-supplied out-of-band public key is trusted. Successful CLI JSON reports whether signature authentication and receipt verification both passed.
- **Key generation** (`capsule keygen`) obtains a raw 32-byte private seed from the OS CSPRNG and derives the matching raw 32-byte public key. It atomically creates new files without overwrite, publishes the harmless public key first, and gives the private file mode `0600` on Unix. If private publication fails, the public file remains and the reported paths permit explicit cleanup.
- **Key reads** use one no-follow/reparse-aware opened handle, add a nonblocking open on Unix, and require a post-open regular file with exact length plus EOF.
- **Key handling.** Private seed buffers are zeroized after generation, derivation, or signing, and keys are never stored in Capsule state. Library callers can use `generate_keypair` or `derive_public_key`.

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

If a capsule path disappears and is replaced with another repository or ordinary directory, `drop --force` still refuses it. Force relaxes the lifecycle requirement; it does not bypass ownership validation.

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

Evidence comes in two kinds, and the distinction is the whole point of the `executed` flag.

**Executed** records come from `EvidenceInput::Run`. Capsule spawns the program itself in the capsule workspace, with no shell, and records the exit status and a digest of the output it captured. A verifier that passes `--require-executed-evidence` is checking that a command really ran and really passed against the exact sealed patch. Verification also refuses any record that asserts `executed` without the captured-output digest and byte count that only an execution produces, so flipping that one field in a receipt does not upgrade a claim.

**Claimed** records come from `EvidenceInput::Claim`. Capsule runs nothing and vouches for nothing. They can never satisfy an executed-evidence requirement.

Execution narrows the trust boundary; it does not remove it. It removes the caller's assertion from the chain, but the exit code and digest were still observed by a Capsule process on the producing host. **A compromised producing host remains outside the boundary**, which is why every attestation keeps `producing-host-was-uncompromised` in `does_not_prove`. Nor is the output digest a reproducibility claim: test output usually contains timings, so re-running the same command legitimately yields a different digest.

Detached receipt signing can authenticate the exact exported bytes to an out-of-band trusted key; it does not make any of the above more true.

## Capability and idempotency limits

`capsule capabilities` negotiates protocol compatibility. It is a static statement about what this build implements, not evidence that the binary is authentic, that its host is trustworthy, or that its state root is usable. It deliberately touches neither state nor Git, so it also cannot tell a caller whether an installation is healthy.

Idempotent creation guarantees at most one capsule identity and worktree per key within one state root. It does not guarantee that the external agent process ran exactly once — Capsule does not launch, observe, or supervise that process. A replay may legitimately return a capsule that is already closed, integrated, orphaned, or dropped, so callers still need targeted lifecycle recovery rather than treating a successful replay as "the work is in progress". Idempotency is local orchestration state and is deliberately absent from portable receipts: a receipt proves result consistency, not agent authorship or execution count.

## Scaling characteristics

These are design properties, not defects, but they surprise operators of large
multi-agent state roots:

- **One reservation per capsule, retained for the life of the state root.** An
  idempotency key is bound permanently so it can never be silently reused, which
  means the index grows exactly in step with capsule records — and those are also
  retained after drop, by design. Both are bounded by `max_capsules`. There is
  deliberately no garbage collector: reclaiming a reservation would make its key
  reusable and break the one guarantee the index exists to provide.
- **Some operations are stop-the-world.** Operations that must see a coherent
  view of every record take the global lock plus
  every known project lock in deterministic order, so they serialise against all
  concurrent capsule mutation. Targeted commands avoid this: `recover <id>`,
  `lookup`, `show`, and `status` read only what they name.
- **`list` is fail-closed and all-or-nothing**, because callers derive
  from it and undercounting would raise an effective quota. Use
  `list --skip-invalid` when you need to see the rest of a root that contains a
  corrupt record.

## Known limits

- A capsule created from a sparse source materializes a complete independent checkout; enabling sparse checkout inside the managed workspace is rejected. Independent temporary indexes make `skip-worktree` and `assume-unchanged` flags irrelevant to snapshots.
- Dirty nested submodule worktrees are rejected because their internal content is not representable by a top-level Git patch; committed gitlink changes remain supported.
- Unregistered embedded Git repositories are rejected rather than silently converted into accidental gitlinks.
- Same-user replacement between individual checks remains possible on hostile filesystems. Close requires matching complete ignored-content inventories around its tracked snapshot transaction, revalidates a second complete tracked snapshot and `HEAD`, and then writes artifacts; cleanup and integration revalidate Git identities immediately before destructive or target-mutating operations. **A hostile same-user mutation after close's final checks remains outside the security boundary:** close is not atomic against such an actor, and the design is not a kernel-enforced capability system.
- On Unix, non-UTF-8 Git inventory paths use an unambiguous raw-byte hex JSON representation; ordinary UTF-8 paths remain strings.
- File locks are advisory.
- Claimed evidence is caller-asserted. Executed evidence is observed by Capsule but still trusts the producing host.
- An executed command inherits the environment and runs with the caller's own privileges. **The worktree is not an execution sandbox**, so run untrusted code under an external one.
- Capsule kills only the process it spawned when a timeout expires. A killed command that left a surviving grandchild holding the output pipe leaves the capture threads waiting on that pipe until it closes. Use process-level isolation if verification commands may outlive their parent.
- Windows ACL hardening is not implemented. Portable metadata checks reject symlink/reparse-like managed entries when exposed as non-regular/non-directory entries, but std does not provide robust owner-private ACL enforcement; Capsule therefore does not claim Windows state is owner-private.

These limits should remain explicit rather than being obscured behind “agent sandbox” terminology.
