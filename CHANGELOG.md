# Changelog

## 0.3.1 — 2026-08-08

Metadata only. No library, CLI, or receipt behaviour changed.

- The GitHub Action is renamed **Change Capsule**, matching the crate, and its
  description is shortened to 112 characters. Marketplace rejects a description
  of 125 or more, and reads both from the tagged `action.yml` rather than from
  the default branch, so listing 0.3.0 was impossible without a new tag.
- The capability-contract test no longer hardcodes the package version, so a
  release bump stops breaking a test whose point is the document's *shape*.

## 0.3.0 — 2026-08-08

This release narrows the crate to two jobs: producing a change an outside party
can re-derive, and proving a verification command really ran. Everything that
did not serve those was removed.

### Added

- **Executed evidence.** `capsule evidence <id> -- cargo test` spawns the program
  directly in the capsule workspace, with no shell, and records the exit code it
  observed plus a domain-separated SHA-256 over the captured stdout and stderr.
  The record carries `executed: true`, and nothing in the API can set that flag
  without an actual run. `--timeout-seconds` kills a command that overruns and
  records nothing. The command runs while no lock is held, so a long test suite
  in one capsule does not serialize every other capsule in the state root.
- **`--require-executed-evidence`** on `close`, `verify`, the merge-gate script,
  and the GitHub Action. It is the only evidence requirement that checks a fact
  rather than a caller's assertion: a `Claim` record can never satisfy it.
- `VerificationReport.evidence_executed` counts records Capsule ran itself.
- Receipt verification and durable-manifest validation now reject any record
  that asserts `executed` without the captured-output digest and byte count that
  only an execution produces. Without this, editing that one boolean in a
  receipt turned a rejected claim into an accepted one at the strictest gate.
- `scripts/self-gate.sh` dogfoods the whole loop in CI on every push.

### Changed

- **`EvidenceInput` is now an enum**, `Run { .. }` or `Claim { .. }`, because
  everything a verifier may conclude depends on which one produced a record.
  Build them with `EvidenceInput::run(argv)` or `EvidenceInput::claim(command,
  exit_code)`. `EvidenceInput::new` is gone.
- `Evidence.patch_sha256` is now required rather than optional, and `Evidence`
  gained `executed`, `output_sha256`, and `output_bytes`.
- The attestation predicate field `claimed_evidence` is now `evidence`, and each
  record carries `executed`. `ProofBoundary::current()` became
  `ProofBoundary::for_receipt(has_executed_evidence)`: a receipt with executed
  evidence moves `evidence-command-actually-ran` out of `does_not_prove` and adds
  `executed-evidence-ran-in-capsule-workspace` to `proves`.
  `producing-host-was-uncompromised` stays disclaimed either way, because
  execution never removes the need to trust the producing host.
- Durable state and receipts are **schema v5**. Schema v3 and v4 are not read.
- `CloseOptions` and `VerifyOptions` gained `requiring(..)` constructors, plus
  `CloseOptions::executed()` and `VerifyOptions::strict(repo)`. These set only
  `require_executed_evidence`. The three requirements are independent, not a
  ladder: `require_successful_evidence` asks that *every* record passed, which
  rejects an attempt whose tests failed once and were then fixed, so folding it
  into the strict presets made the strongest mode unusable for the ordinary
  agent loop.
- The GitHub Action now defaults `require-executed-evidence` to `true` and
  `require-successful-evidence` to `false`, for the same reason.
- The README leads with the argument that makes this crate different: a source
  change can be re-derived, unlike a build artifact, so a verifier recomputes
  rather than trusts.

### Removed

Roughly 3,000 lines, none of which had users:

- **The policy engine** (`Policy`, `PolicyReport`, `capsule policy`, repository
  allowlists, and every count/age/byte/path quota). A fixed 64 MiB patch safety
  bound remains as `HARD_PATCH_BYTES`. Use OS quotas and process isolation for
  real limits; the crate never enforced them anyway.
- **The audit log and metrics** (`AuditEvent`, `AuditEventKind`,
  `MetricsSnapshot`, `capsule audit`, `capsule metrics`, and the per-manifest
  event ring). Lifecycle state is already in the manifest.
- **State administration** (`capsule state inspect`, `capsule state backup`,
  `StateInspection`, `BackupReport`).
- **The v3-to-v4 migration** (`capsule state migrate`, `MigrationReport`,
  rollback journals, `LEGACY_SCHEMA_VERSION`). The crate is days old; carrying
  migration machinery cost more than it saved.
- **The committed-receipt protocol**: `receipts/`, the `receipt-gate` job,
  `scripts/prepare-committed-receipt.sh`, `scripts/test-committed-receipt.sh`,
  `scripts/check-release-pins.sh`, and `docs/releasing.md`. It taxed every
  contributor a second commit and forbade squash merges to prove something
  `scripts/self-gate.sh` now proves in CI without touching the commit graph.
- `Error::PolicyViolation` and the `policy` CLI error kind.

## 0.2.0 — 2026-08-08

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
- `capsule attest` emits a standard [in-toto Statement v1](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md) for a verified receipt, so teams already running in-toto, SLSA, or Sigstore consume Capsule output through existing tooling instead of adopting a second format. The subject carries the sealed patch by `sha256` and the result `HEAD` by `gitCommit`; the predicate is versioned independently at `https://github.com/SiliconState/change-capsule/attestation/change/v1`. `--predicate-only` feeds `cosign attest-blob` directly. Attestation verifies the receipt first and refuses to emit for one that does not verify, so it cannot launder a tampered bundle. Library entry points are `attest_bundle` and `change_statement`; `docs/interop.md` explains the design, including why signing covers exact bytes rather than a canonicalised object.
- Every attestation carries a machine-readable `proof_boundary`, and evidence is named `claimed_evidence`, so no consumer has to read prose to learn which claims are established and which are merely asserted.
- `capsule list --skip-invalid` (`CapsuleManager::list_reporting`) reports unreadable records instead of failing on the first one. The strict path stays fail-closed because policy counts derive from it.
- Public structs and enums are now `#[non_exhaustive]`, so future fields and variants are additive rather than breaking. Option types gained constructors and `with_*` builders (`Author::new`, `CheckpointOptions::new`, `EvidenceInput::new`/`with_summary`, `IntegrateOptions::new`/`with_message`, `CloseOptions::new`, `VerifyOptions::new`, `CreateOptions::with_base`/`with_label`/`with_links`).

### Changed

- Capsule creation now materializes a complete workspace from sparse source worktrees. Independent temporary indexes no longer reject `skip-worktree` or `assume-unchanged` flags.
- Dirty submodules and unregistered embedded repositories remain fail-closed.
- `manager.rs` was split into a `manager/` module directory (`create`, `lifecycle`, `query`, `recover`, `artifacts`, `admin`, `tests`) with no behaviour change; the largest file dropped from about 3,700 lines to about 1,000.
- CI gained MSRV verification against the declared `rust-version`, coverage via `cargo-llvm-cov`, a fuzz-target build, and a release-pin consistency check. `fuzz/` adds libFuzzer targets for receipt decoding, receipt verification, and `GitPath` canonicality; `benches/` adds criterion benchmarks; property tests prove the canonical request digest cannot be confused by field concatenation.
- Added `SECURITY.md`, `CONTRIBUTING.md`, issue and pull-request templates, Dependabot configuration, `docs/interop.md`, and `docs/releasing.md`.
- Dependencies updated across two semver-major crypto-stack revisions: `ed25519-dalek` 3.0 (curve25519-dalek 5), `sha2` 0.11 (digest 0.11), and `getrandom` 0.4, plus `criterion` 0.7 and GitHub Actions `checkout@v7`/`cache@v6`. No source changes were required, the declared 1.85 MSRV still verifies, and `cargo audit`/`cargo deny` stay clean.
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
