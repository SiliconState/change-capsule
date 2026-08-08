## What this changes

<!-- One or two sentences. Link the issue if there is one. -->

## Why

<!-- What problem does this solve? If it narrows or widens what Capsule proves,
say so, and update docs/security.md in this same change. -->

## Checks

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --all-features --locked`
- [ ] `CAPSULE_BIN="$PWD/target/debug/capsule" bash scripts/self-gate.sh`
- [ ] New safety behaviour has a regression test that fails without the fix.

Cross-compiling catches most platform breakage before CI does:

```sh
cargo clippy --target x86_64-pc-windows-msvc --all-targets --all-features -- -D warnings
cargo clippy --target x86_64-apple-darwin --all-targets --all-features -- -D warnings
```
