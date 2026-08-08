# Contributing

## How this repository proves its own tool works

CI runs [`scripts/self-gate.sh`](scripts/self-gate.sh), which drives the whole
loop against this repository on every push: it creates a real capsule, changes a
file, has Capsule execute `cargo test`, seals with `--require-executed-evidence`,
exports a receipt, verifies it from a fresh clone, and confirms a tampered
receipt is rejected.

Nothing about that touches your commits. Branch, commit, and merge however you
like.

## Local checks

Run what CI runs, before pushing:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo build --release --locked
CAPSULE_BIN="$PWD/target/debug/capsule" bash scripts/self-gate.sh
```

**Cross-check the other platforms.** A fully green Linux run has broken `main`
twice. Clippy only type-checks, so no cross-linker is needed:

```sh
rustup target add x86_64-pc-windows-msvc x86_64-apple-darwin
cargo clippy --target x86_64-pc-windows-msvc --all-targets --all-features --locked -- -D warnings
cargo clippy --target x86_64-apple-darwin  --all-targets --all-features --locked -- -D warnings
```

Two runtime traps cross-compilation cannot catch:

- macOS rejects non-UTF-8 filenames with `EILSEQ`, so fixtures creating them must
  be `#[cfg(target_os = "linux")]`, not `#[cfg(unix)]`.
- `--json` escapes `\`, so never match a Windows path against raw stderr bytes;
  parse the JSON and assert on the decoded message.

The crate also declares an MSRV in `Cargo.toml`, enforced by the `msrv` job.

## House style

- `unsafe_code` is **forbidden**, and clippy runs at `pedantic` denied. Prefer
  restructuring over an `#[allow]`; when an allow is genuinely right, comment why.
- **Fail closed.** Ambiguous state is an error, never a guess. Never delete or
  overwrite to make an operation succeed.
- **Public API is `#[non_exhaustive]`.** Adding a field or variant must not be a
  breaking change. Build option types with their constructors and `with_*`
  builders rather than struct literals.
- Comments explain *why*. The code already says what.
- Documentation states real limits. If a change narrows or widens what Capsule
  proves, update `docs/security.md` in the same change.

## Tests

- `tests/capsule.rs` — lifecycle, sealing, receipts, signing.
- `tests/executed_evidence.rs` — executed evidence: the run/claim distinction and its verification.
- `tests/orchestration.rs` — capabilities and idempotency protocol.
- `tests/attestation.rs` — in-toto conformance and the proof boundary.
- `fuzz/` — untrusted-input targets; CI builds them, it does not run campaigns.

New safety behaviour needs a regression that fails without the fix. Verify that
it does before submitting.

## Security issues

Do not open a public issue. Follow [`SECURITY.md`](SECURITY.md).
