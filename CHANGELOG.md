# Changelog

## 0.2.0 — Unreleased

### Added

- Schema-v4 evidence binds caller-claimed command outcomes to the complete patch SHA-256 observed when evidence is attached. Close and receipt verification can require successful evidence current for the sealed patch, separately from legacy successful-evidence policy.
- Explicit schema-v3 state migration supports dry-run and backup-required apply reports. Apply completes an external backup before transactional journaled writes; v3 evidence migrates unbound. Exported v3 receipts remain verifiable.
- Optional detached Ed25519 signatures authenticate a fixed-domain SHA-256 commitment of exact `bundle.json` bytes with verifier-supplied trusted public keys. `capsule keygen` creates matching raw 32-byte private-seed/public-key files from the OS CSPRNG without overwrite (Unix private mode `0600`), and library key generation/derivation APIs are public.
- Unix Git inventories losslessly encode non-UTF-8 pathname bytes while retaining JSON strings for ordinary UTF-8 paths. Raw-byte encodings are canonical lowercase hex and cannot duplicate valid UTF-8 identities; ignored provenance hashes native names and symlink targets under an explicit platform/version domain.
- JSON diff metadata now includes canonical lowercase `patch_sha256` for the exact live/sealed patch returned or written, enabling current-evidence deduplication.
- `capsule capabilities` (`Capabilities::current()`) emits a static, bounded, deterministic machine-readable protocol contract: an independent capability schema version, protocol versions, stable versioned feature identifiers, supported durable/receipt/bundle/idempotency-record schemas, and byte limits. It runs before state or manager initialization, never touches `CAPSULE_HOME` or Git, and succeeds against a missing, unwritable, malformed, or incompatible state root. Capabilities negotiate protocol compatibility only, never trust or binary authenticity.
- State-root-scoped crash-safe idempotent creation: `capsule create --idempotency-key <key>` and `CapsuleManager::create_idempotent`. A durable reservation binding the reserved capsule ID, canonical repository identity, original base selector, resolved immutable base commit, label, and links is published atomically without overwrite before the first capsule-directory, branch, worktree, or manifest side effect, so retries after timeout or crash resume the same identity. The same key with a materially different request fails with the new `idempotency_conflict` error kind before any second side effect, and repeating a selector — including `HEAD` — never retargets an existing key to a newer commit. `create` without the flag keeps its existing behavior and response, and `CreateOptions` is unchanged.
- Direct keyed lookup: `capsule lookup --idempotency-key <key>`, `CapsuleManager::lookup_idempotency_key`, and manager-free `lookup_idempotency_key_at`. Lookup is non-mutating, reads only the hashed reservation path and the capsule it references, reports `reserved` or `materialized`, stays usable when unrelated records are malformed, and returns the new `idempotency_not_found` kind without echoing the raw key. This removes full-state discovery scans from the normal crash-safe creation path.
- Reservations live in a private `idempotency/` index keyed by a domain-separated SHA-256 of the key, with its own schema version 1 independent of the durable capsule schema. State-byte accounting, backup, and `state inspect` now cover the index; inspection reports malformed entries by indexed digest without requiring capsule schema compatibility. Idempotency keys and their request metadata never appear in portable receipts.

### Changed

- Capsule creation now materializes a complete workspace from sparse source worktrees. Independent temporary indexes no longer reject `skip-worktree` or `assume-unchanged` flags.
- Dirty submodules and unregistered embedded repositories remain fail-closed.
- CI dogfood keeps the published-action compatibility gate pinned to published 0.1.2 while also verifying the committed receipt with the current 0.2.0 source binary; dependency audit/license checks were added.

### Security

- Authenticated verification now authenticates and verifies one exact in-memory `bundle.json` snapshot through a single public API; the CLI no longer reopens the bundle between checks.
- Migration commit uses an atomic active-journal to committed-cleanup namespace transition, so recovery cannot mistake partial journal deletion for an uncommitted migration.
- Key reads use one no-follow/reparse-aware opened handle, add nonblocking opens on Unix, and retain post-open regular-file plus exact-length-and-EOF validation. Key generation publishes the public key before the private seed, and detached signatures are staged, synced, and atomically published without overwrite.
- Recovery can target one known capsule without scanning unrelated records (`CapsuleManager::recover_capsule` and `capsule recover ID`).
- Migration journal recovery now runs only while holding the same cross-process global lock as migration and initialization. Migration validates result/patch bytes and rewrites and verifies the complete capsule `result` reference before backup or mutation.
- Close now requires two complete ignored-content inventories around its tracked snapshot transaction to match exactly on lossless path identities, byte totals, and structural digest; it uses the stable final inventory for policy and the sealed result. It also revalidates a second complete tracked snapshot, changed-path inventory, `HEAD`, and clean/dirty result-kind classification, binds current evidence to the exact final patch, and fails before artifact writes on instability. Hostile same-user mutation after final checks remains outside the security boundary.
- Bounded state and artifact reads now open once with no-follow/reparse-aware flags plus nonblocking opens on Unix, validate the opened regular-file handle and size, and read bounded bytes from that same handle so FIFOs and other special files cannot wedge a read. CLI policy-file reads use the same safe bounded pattern, and lock creation validates and hardens the opened descriptor.
- Schema-v3 migration rejects mixed-schema pairs and capsule/result invariant mismatches before backup or writes; dry-run rejects backup arguments and never reports a backup directory. Schema-v4 state and receipt verification require the ignored-content digest and canonical lowercase identities/digests.
- Idempotency reservations reuse the existing owner-private, no-follow/reparse-aware, bounded state-file protections and are published atomically without overwrite, file-synced, and parent-directory-synced. A raw key is never a filename. Reads reject symlinks, reparse points, special files, malformed or oversized records, filename/digest disagreement, path substitution, request-digest mismatch, and reservation/capsule identity mismatch, and never silently rebuild or overwrite a conflicting record. An unrecoverable or contradictory first attempt stays bound to its key and is marked orphaned rather than replaced. `recover_creating` now completes an interrupted creation only when the durable manifest and the absence or exact agreement of Git branch/worktree state prove it safe, and never deletes ambiguous paths or branches to make recovery succeed.

## 0.1.2 — 2026-08-06

### Fixed

- Exported receipts embedded the absolute path of the machine that produced
  them in `bundle.json`. A receipt is meant to travel, so committing one to a
  repository or publishing it to an artifact store disclosed the exporting
  machine's directory layout, and the recorded location was meaningless to
  whoever verified the receipt elsewhere. Exported artifacts are now referenced
  by name, relative to the bundle directory. Verification is unaffected: it has
  always matched artifacts by name, digest, and byte count. Artifacts still held
  in capsule state keep their absolute local `file://` URIs.

## 0.1.1 — 2026-08-06

Bug fixes. Upgrading is recommended for anyone creating checkpoints.

### Fixed

- A capsule could permanently wedge itself. Checkpoints had no growth bound, so
  a long-running attempt could reach a point where the branch advanced but the
  manifest recording it exceeded the durable storage cap. The capsule was then
  stuck in `checkpointing`, and every `capsule recover` retried the same
  oversized write and failed identically. Checkpointing now proves both
  manifests it persists still fit before taking any irreversible Git side
  effect, and refuses cleanly instead, leaving the capsule active and sealable.
  A capsule also retains at most 128 checkpoints.
- Recording evidence now verifies the encoded manifest size rather than only the
  raw input size, so JSON escaping cannot push a manifest past its cap.
- `Cleanup.branch_head` serialized an explicit `null` instead of being omitted.

## 0.1.0 — 2026-08-06

First public release: agent-neutral, isolated code-change attempts backed by
ordinary Git worktrees, sealed into portable, verifiable receipts.

- Isolated capsules pinned to an exact base commit; parallel attempts never
  race on one checkout and never touch the primary worktree.
- Complete binary-capable patch, changed-path inventory, checkpoints,
  caller-recorded evidence, and content digests sealed at close.
- `capsule export` produces a self-describing receipt (`bundle.json`,
  `result.json`, `result.patch`); `capsule verify` re-checks it anywhere with
  no capsule state, and `--repo` proves the sealed patch reproduces exactly
  against the pinned base.
- Journaled, crash-recoverable create/checkpoint/integrate/drop transitions;
  `capsule recover` completes only provable transitions.
- Drift detection covers tracked content; the ignored-content inventory is
  sealed at close as provenance and its later churn (build output, caches)
  does not block integration or cleanup.
- Repository allowlists and resource limits enforced at mutation boundaries;
  usage that no configured limit references is never measured.
- Evidence is bounded (64 records, 256 KiB total) so the durable manifest
  cannot outgrow its own storage cap.
- GitHub Action merge gate at the repository root: verifies a receipt in CI and
  can refuse a merge unless the tree equals the pinned base plus the sealed
  patch. Runs locally too, via `scripts/verify-gate.sh`.
- Runnable demo: `examples/parallel-attempts.sh`.
