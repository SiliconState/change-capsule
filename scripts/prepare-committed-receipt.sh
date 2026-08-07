#!/usr/bin/env bash
# Validate the two-commit committed-receipt protocol and materialize the exact
# implementation tree that a receipt claims to describe.
set -euo pipefail

bundle=""
output=""
revision="HEAD"
repo="."

die() { printf '%s\n' "error: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundle) bundle="${2:-}"; shift 2 ;;
    --output) output="${2:-}"; shift 2 ;;
    --revision) revision="${2:-}"; shift 2 ;;
    --repo) repo="${2:-}"; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$bundle" ]] || die "--bundle is required"
[[ -n "$output" ]] || die "--output is required"
[[ "$bundle" != /* ]] || die "--bundle must be relative to the repository"
bundle="${bundle#./}"
bundle="${bundle%/}"
[[ -n "$bundle" && "$bundle" != "." ]] || die "--bundle must name a receipt directory"
case "/$bundle/" in
  *'/../'*|*'/./'*|*'//'*) die "--bundle must be a normalized relative path" ;;
esac

repo=$(CDPATH='' cd -- "$repo" && pwd -P)
git -C "$repo" rev-parse --git-dir > /dev/null 2>&1 || die "not a Git repository: $repo"
[[ -z "$(git -C "$repo" status --porcelain)" ]] || die "working tree must be clean"
revision=$(git -C "$repo" rev-parse --verify "${revision}^{commit}") || die "invalid revision"

read -r -a record <<< "$(git -C "$repo" rev-list --parents -n 1 "$revision")"
[[ ${#record[@]} -eq 2 ]] || die "receipt revision must have exactly one parent"
parent="${record[1]}"
read -r -a implementation_record <<< "$(git -C "$repo" rev-list --parents -n 1 "$parent")"
[[ ${#implementation_record[@]} -eq 2 ]] || {
  die "implementation revision must have exactly one parent"
}
implementation_base="${implementation_record[1]}"

expected=(
  "$bundle/bundle.json"
  "$bundle/result.json"
  "$bundle/result.patch"
)
mapfile -d '' changed < <(
  git -C "$repo" diff-tree --no-commit-id --name-only -r -z "$revision"
)
[[ ${#changed[@]} -eq ${#expected[@]} ]] || {
  die "receipt commit must change only bundle.json, result.json, and result.patch under $bundle"
}

declare -A allowed=()
for path in "${expected[@]}"; do
  allowed["$path"]=1
  mode=$(git -C "$repo" ls-tree "$revision" -- "$path" | awk '{print $1}')
  [[ "$mode" == "100644" || "$mode" == "100755" ]] || {
    die "receipt artifact is missing or not a regular file: $path"
  }
done
for path in "${changed[@]}"; do
  [[ -n "${allowed[$path]:-}" ]] || {
    die "receipt commit contains a non-envelope change: $path"
  }
done
mapfile -d '' bundled < <(
  git -C "$repo" ls-tree -r --name-only -z "$revision" -- "$bundle"
)
[[ ${#bundled[@]} -eq ${#expected[@]} ]] || {
  die "receipt directory must contain exactly bundle.json, result.json, and result.patch"
}
for path in "${bundled[@]}"; do
  [[ -n "${allowed[$path]:-}" ]] || {
    die "receipt directory contains an unexpected path: $path"
  }
done

[[ ! -e "$output" && ! -L "$output" ]] || die "output path already exists: $output"
mkdir -p "$(dirname -- "$output")"
git -C "$repo" worktree add --quiet --detach "$output" "$parent"
output=$(CDPATH='' cd -- "$output" && pwd -P)

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  printf 'repo=%s\n' "$output" >> "$GITHUB_OUTPUT"
  printf 'receipt-parent=%s\n' "$parent" >> "$GITHUB_OUTPUT"
  printf 'implementation-base=%s\n' "$implementation_base" >> "$GITHUB_OUTPUT"
fi
printf 'Committed receipt envelope verified; sealed tree: %s (%s)\n' "$output" "$parent"
