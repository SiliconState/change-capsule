#!/usr/bin/env bash
# Dogfood the whole loop against this repository, in one CI job.
#
# CI runs this on every push. It creates a real capsule, makes a real change,
# has Capsule *execute* the verification command, seals, exports, and then
# verifies the receipt at the strictest level available, from a fresh clone.
#
#   scripts/self-gate.sh
#
# Nothing is pushed and the working tree is left untouched.
set -euo pipefail

capsule_bin="${CAPSULE_BIN:-capsule}"
command -v "$capsule_bin" > /dev/null 2>&1 || {
  printf 'error: capsule binary not found: %s\n' "$capsule_bin" >&2
  exit 1
}
command -v jq > /dev/null 2>&1 || { printf 'error: jq is required\n' >&2; exit 1; }

scratch=$(mktemp -d)
export CAPSULE_HOME="$scratch/state"
id=""

# A capsule's worktree lives under CAPSULE_HOME, but its registration and branch
# live in *this* repository. Deleting the scratch directory alone would leave a
# prunable worktree and a `capsule/<ulid>` branch behind on every run, so drop
# the capsule first and only then remove the state.
cleanup() {
  if [[ -n "$id" ]]; then
    "$capsule_bin" drop "$id" --force > /dev/null 2>&1 || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

step() { printf '\n=== %s\n' "$1"; }

step "create an isolated attempt from the current HEAD"
capsule_json=$("$capsule_bin" --json create --repo . --label "self gate" --link ci=self-gate)
id=$(printf '%s' "$capsule_json" | jq -r .id)
workspace=$(printf '%s' "$capsule_json" | jq -r .workspace_path)
base=$(printf '%s' "$capsule_json" | jq -r .base_commit)
printf 'capsule %s at %s (base %s)\n' "$id" "$workspace" "$base"

step "make a change inside the attempt"
printf '\n<!-- self-gate touched this line -->\n' >> "$workspace/README.md"

step "have Capsule run the verification command itself"
# The command runs inside the capsule workspace. Capsule observes the exit code
# and output; nothing here can assert a result it did not see.
#
# Build outside the workspace, and by default into this repository's own target
# directory. Two reasons. `target/` is git-ignored, so it never enters the patch,
# but close still hashes every ignored byte to record what it excluded, and
# several gigabytes of build output would be read twice for no benefit. Reusing
# the ordinary target directory also means CI's Rust cache applies, instead of a
# cold dependency build on every run. This has to be exported, not prefixed onto
# the assignment below, or the child never sees it.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
evidence=$("$capsule_bin" --json evidence "$id" -- cargo test --all-features --quiet)
printf '%s' "$evidence" | jq -e '.executed == true' > /dev/null \
  || { printf 'error: evidence was not executed by Capsule\n' >&2; exit 1; }
printf '%s' "$evidence" | jq -e '.exit_code == 0' > /dev/null \
  || { printf 'error: the verification command failed\n' >&2; exit 1; }
printf 'executed evidence: %s bytes of output, digest %s\n' \
  "$(printf '%s' "$evidence" | jq -r .output_bytes)" \
  "$(printf '%s' "$evidence" | jq -r .output_sha256)"

step "seal, requiring evidence Capsule ran itself"
"$capsule_bin" --json close "$id" --require-executed-evidence > /dev/null

step "export a portable receipt"
"$capsule_bin" --json export "$id" --output "$scratch/receipt" > /dev/null

step "verify the receipt the way a merge gate would"
# A fresh clone stands in for the reviewer's machine: the receipt must verify
# against a checkout that never saw the capsule state directory.
git clone --quiet --no-local . "$scratch/reviewer"
CAPSULE_BIN="$capsule_bin" bash "$(dirname "$0")/verify-gate.sh" \
  --bundle "$scratch/receipt" \
  --repo "$scratch/reviewer" \
  --require-executed-evidence

step "confirm a tampered receipt is rejected"
printf 'tamper\n' >> "$scratch/receipt/result.patch"
if "$capsule_bin" --json verify "$scratch/receipt" > /dev/null 2>&1; then
  printf 'error: a tampered receipt verified\n' >&2
  exit 1
fi

printf '\nself-gate passed: %s sealed, exported, verified, and tamper-checked\n' "$id"
