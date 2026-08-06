#!/usr/bin/env bash
# Merge-gate verification for a Capsule receipt.
#
# Verifies an exported bundle and, optionally, proves that the currently
# checked-out tree is exactly the pinned base plus the sealed patch. Used by
# the repository-root composite action, and runnable directly:
#
#   scripts/verify-gate.sh --bundle ./receipt --repo . --verify-head
#
# Emits GitHub Actions outputs and a step summary when those environment
# variables are present, and is silent about them otherwise.
set -euo pipefail

bundle=""
repo="."
require_evidence="false"
verify_head="false"
capsule_bin="${CAPSULE_BIN:-capsule}"

die() { printf '%s\n' "error: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundle) bundle="${2:-}"; shift 2 ;;
    --repo) repo="${2:-}"; shift 2 ;;
    --capsule-bin) capsule_bin="${2:-}"; shift 2 ;;
    --require-successful-evidence) require_evidence="true"; shift ;;
    --verify-head) verify_head="true"; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$bundle" ]] || die "--bundle is required"
[[ -d "$bundle" ]] || die "bundle directory not found: $bundle"
command -v "$capsule_bin" > /dev/null 2>&1 || die "capsule binary not found: $capsule_bin"
command -v jq > /dev/null 2>&1 || die "jq is required"

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

emit() { # name value
  [[ -n "${GITHUB_OUTPUT:-}" ]] && printf '%s=%s\n' "$1" "$2" >> "$GITHUB_OUTPUT"
  return 0
}

summary() {
  [[ -n "${GITHUB_STEP_SUMMARY:-}" ]] && printf '%s\n' "$1" >> "$GITHUB_STEP_SUMMARY"
  return 0
}

fail() {
  emit verified false
  summary "### ❌ Capsule receipt rejected"
  summary ""
  summary "$1"
  die "$1"
}

verify_args=("verify" "$bundle" "--repo" "$repo")
[[ "$require_evidence" == "true" ]] && verify_args+=("--require-successful-evidence")

report=""
if ! report=$("$capsule_bin" --json "${verify_args[@]}" 2>"$scratch/verify-err"); then
  detail=$(jq -r '.error // empty' < "$scratch/verify-err" 2>/dev/null || true)
  [[ -n "$detail" ]] || detail=$(cat "$scratch/verify-err")
  fail "${detail}"
fi

capsule_id=$(jq -r '.capsule_id' <<< "$report")
base_commit=$(jq -r '.base_commit' <<< "$report")
patch_sha256=$(jq -r '.patch_sha256' <<< "$report")
changed_paths=$(jq -r '.changed_paths' <<< "$report")
patch_bytes=$(jq -r '.patch_bytes' <<< "$report")
evidence_total=$(jq -r '.evidence_total' <<< "$report")
evidence_failed=$(jq -r '.evidence_failed' <<< "$report")
kind=$(jq -r '.kind' <<< "$report")

emit capsule-id "$capsule_id"
emit base-commit "$base_commit"
emit patch-sha256 "$patch_sha256"
emit changed-paths "$changed_paths"

head_state="not requested"
if [[ "$verify_head" == "true" ]]; then
  git -C "$repo" rev-parse --git-dir > /dev/null 2>&1 || fail "not a Git repository: $repo"
  if [[ -n "$(git -C "$repo" status --porcelain)" ]]; then
    fail "working tree is not clean; head verification needs a clean checkout"
  fi
  if ! git -C "$repo" rev-parse --verify --quiet "${base_commit}^{commit}" > /dev/null; then
    fail "pinned base ${base_commit} is not present in this checkout; fetch full history (fetch-depth: 0)"
  fi
  # Mirrors the flags Capsule itself uses to build a sealed patch, so a
  # matching tree produces byte-identical output.
  git -C "$repo" diff --binary --full-index --no-ext-diff --no-textconv \
    --no-renames --no-color "$base_commit" HEAD -- > "$scratch/head.patch"
  if cmp -s "$scratch/head.patch" "$bundle/result.patch"; then
    head_state="matches the sealed patch"
  else
    fail "the checked-out tree is not the pinned base plus the sealed patch; \
the diff being merged differs from the receipt"
  fi
fi

emit verified true

summary "### ✅ Capsule receipt verified"
summary ""
summary "| Field | Value |"
summary "| --- | --- |"
summary "| Capsule | \`${capsule_id}\` |"
summary "| Result kind | ${kind} |"
summary "| Pinned base | \`${base_commit}\` |"
summary "| Patch digest | \`sha256:${patch_sha256}\` |"
summary "| Patch size | ${patch_bytes} bytes across ${changed_paths} path(s) |"
summary "| Evidence | ${evidence_total} record(s), ${evidence_failed} failing |"
summary "| Merged tree | ${head_state} |"

printf 'Capsule receipt verified: %s (%s path(s), head %s)\n' \
  "$capsule_id" "$changed_paths" "$head_state"
