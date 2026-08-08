#!/usr/bin/env bash
# Keep the published-action pins and the committed receipt schema consistent.
#
# CI's receipt gate installs a *published* `change-capsule` and verifies the
# committed receipt with it. A published binary only decodes the durable schema
# it shipped with, so the pinned version and the committed receipt's
# `schema_version` are a matched pair. Releasing moves both, together; moving
# one alone turns main red for everybody.
set -euo pipefail

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*"; }

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$repo"

# Highest durable result schema each published series can decode.
# 0.1.x predates schema v4 and rejects it outright.
max_schema_for() {
  case "$1" in
    0.1.*) echo 3 ;;
    0.2.*) echo 4 ;;
    *) die "unknown published series '$1'; extend max_schema_for in $0" ;;
  esac
}

action_default=$(sed -n 's/^[[:space:]]*default: "\([0-9][^"]*\)".*/\1/p' action.yml | tail -n 1)
[[ -n "$action_default" ]] || die "could not read the version default from action.yml"

ci_version=$(sed -n 's/^[[:space:]]*version: "\([0-9][^"]*\)".*/\1/p' .github/workflows/ci.yml | tail -n 1)
[[ -n "$ci_version" ]] || die "could not read the pinned version from .github/workflows/ci.yml"

note "action.yml default: $action_default"
note "ci.yml receipt gate: $ci_version"

[[ "$action_default" == "$ci_version" ]] || die \
  "action.yml pins $action_default but ci.yml pins $ci_version"

# Documentation references the action by tag, which must match the same series.
doc_tags=$(grep -rhoE 'SiliconState/change-capsule@v[0-9][0-9.]*' README.md docs/*.md 2>/dev/null \
  | sed 's/.*@v//' | sort -u || true)
for tag in $doc_tags; do
  # `@v0` style major tags always float forward and are fine.
  [[ "$tag" == *.* ]] || continue
  [[ "$tag" == "$ci_version" ]] || die \
    "documentation references @v$tag but CI pins $ci_version"
done
note "documentation tags: ${doc_tags:-none} (ok)"

receipt=receipts/required/result.json
[[ -f "$receipt" ]] || die "missing committed receipt at $receipt"
schema=$(sed -n 's/.*"schema_version"[[:space:]]*:[[:space:]]*\([0-9]\+\).*/\1/p' "$receipt" | head -n 1)
[[ -n "$schema" ]] || die "could not read schema_version from $receipt"

supported=$(max_schema_for "$ci_version")
note "committed receipt schema: $schema (pinned $ci_version decodes up to $supported)"

if (( schema > supported )); then
  die "committed receipt is schema v$schema but the pinned published binary \
($ci_version) only decodes up to v$supported; see docs/releasing.md"
fi

printf 'release pins are consistent\n'
