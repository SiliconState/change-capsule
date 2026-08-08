# Releasing

Capsule dogfoods its own merge gate, which makes releasing unusual: CI verifies
the committed receipt with a **published** binary, so the release changes the
tool that validates the repository. Two things must move together or `main`
goes red.

`scripts/check-release-pins.sh` enforces the invariant on every CI run, and the
`release-pins` job fails the build if it drifts. Read this before changing any
version number.

## The coupled pair

1. **The published-action pin** — the same version in three places:
   - `action.yml`, the `version` input default;
   - `.github/workflows/ci.yml`, the `receipt-gate` job's `version:`;
   - `README.md` and `docs/composition.md`, which reference
     `SiliconState/change-capsule@vX.Y.Z`.
2. **The committed receipt's `schema_version`** in `receipts/required/result.json`.

The gate runs `cargo install change-capsule --version <pin>` and verifies the
committed receipt with it. A published binary decodes only the durable schema
it shipped with:

| Published series | Decodes result schema |
| --- | --- |
| `0.1.x` | 3 |
| `0.2.x` | 3 and 4 |

So while the pin is `0.1.2`, the committed receipt **must stay schema v3**, which
means sealing it with the `0.1.2` binary rather than a source build. Bumping the
crate version alone does not change this; only publishing does.

## Releasing 0.2.0

Order matters. Steps 4 and 5 are the coupled pair and belong in the same change.

1. Confirm `CHANGELOG.md` has a dated `0.2.0` section and that `Cargo.toml`
   already reads `version = "0.2.0"`.
2. Run the full local gate: `cargo fmt --all -- --check`,
   `cargo clippy --all-targets --all-features --locked -- -D warnings`,
   `cargo test --all-features --locked`, `cargo build --release --locked`,
   `cargo audit --deny warnings`,
   `cargo deny check advisories bans licenses sources`, plus
   `cargo clippy --target x86_64-pc-windows-msvc ...` and
   `--target x86_64-apple-darwin ...` — a Linux-only pass has broken `main`
   before.
3. Tag and publish. The `release` workflow publishes through OIDC trusted
   publishing; no long-lived token exists or should be created.
4. **Only after `0.2.0` is on crates.io**, move the pin to `0.2.0` in
   `action.yml`, `.github/workflows/ci.yml`, `README.md`, and
   `docs/composition.md`.
5. **In the same change**, reseal the committed receipt with the current source
   binary so it becomes schema v4, following the two-commit protocol in
   [`../README.md`](../README.md#committed-receipt-protocol).
6. Run `bash scripts/check-release-pins.sh` locally before pushing. It refuses a
   receipt newer than the pinned binary can read, and refuses pins that disagree
   with each other.

## Why not just float the pin

Pinning a concrete version is what lets the Action's binary cache engage, and it
is also what makes the gate a genuine *compatibility* test: it proves the last
published release can still verify what this branch produces. Floating it would
hide exactly the break this repository most needs to catch.

## Adding a new published series

Extend `max_schema_for` in `scripts/check-release-pins.sh` with the new series
and the highest durable schema it decodes. The script fails closed on an unknown
series rather than guessing.
