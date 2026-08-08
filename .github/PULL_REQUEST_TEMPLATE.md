## What this changes

<!-- What behaviour differs after this lands, and why. -->

## Receipt protocol

This repository lands changes as two commits: the implementation, then a commit
adding only `receipts/required/{bundle.json,result.json,result.patch}`. See
[CONTRIBUTING.md](../CONTRIBUTING.md).

- [ ] The tip commit changes only the three receipt artifacts
- [ ] The receipt's pinned base equals this PR's base commit
- [ ] No amend/squash/rebase since the receipt was sealed
- [ ] Evidence recorded in the receipt reflects commands actually run

## Checks

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --all-features --locked`
- [ ] Cross-target clippy for `x86_64-pc-windows-msvc` and `x86_64-apple-darwin`
- [ ] `bash scripts/check-release-pins.sh`

## Safety

- [ ] New safety behaviour has a regression that fails without the fix
- [ ] Public API additions are `#[non_exhaustive]`-compatible
- [ ] `docs/security.md` updated if what Capsule proves changed
