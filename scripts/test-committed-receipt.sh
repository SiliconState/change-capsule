#!/usr/bin/env bash
# Deterministic adversarial tests for prepare-committed-receipt.sh.
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
binder="$script_dir/prepare-committed-receipt.sh"
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
repo="$root/repo"
bundle="receipts/test"

mkdir "$repo"
git -C "$repo" init -q -b main
git -C "$repo" config user.name "Receipt Test"
git -C "$repo" config user.email "receipt@example.test"
printf 'base\n' > "$repo/app.txt"
git -C "$repo" add .
git -C "$repo" commit -qm "base"

printf 'sealed implementation\n' > "$repo/app.txt"
git -C "$repo" add app.txt
git -C "$repo" commit -qm "implementation"
implementation=$(git -C "$repo" rev-parse HEAD)

mkdir -p "$repo/$bundle"
printf '{}\n' > "$repo/$bundle/bundle.json"
printf '{}\n' > "$repo/$bundle/result.json"
printf 'patch\n' > "$repo/$bundle/result.patch"
git -C "$repo" add "$bundle"
git -C "$repo" commit -qm "receipt envelope"

sealed_tree="$root/sealed-tree"
"$binder" --repo "$repo" --bundle "$bundle" --output "$sealed_tree" > /dev/null
[[ "$(git -C "$sealed_tree" rev-parse HEAD)" == "$implementation" ]]
[[ "$(cat "$sealed_tree/app.txt")" == "sealed implementation" ]]
[[ ! -e "$sealed_tree/$bundle" ]]
git -C "$repo" worktree remove --force "$sealed_tree"

# A receipt commit carrying even one unrelated path must not be able to bless it.
git -C "$repo" switch -q -C malicious "$implementation"
mkdir -p "$repo/$bundle"
printf '{}\n' > "$repo/$bundle/bundle.json"
printf '{}\n' > "$repo/$bundle/result.json"
printf 'patch\n' > "$repo/$bundle/result.patch"
printf 'smuggled\n' > "$repo/surprise.txt"
git -C "$repo" add .
git -C "$repo" commit -qm "receipt plus unsealed payload"
if "$binder" --repo "$repo" --bundle "$bundle" --output "$root/rejected" > /dev/null 2>&1; then
  echo "binder accepted an unrelated payload" >&2
  exit 1
fi

# A stale extra file under the receipt directory is rejected even when the tip
# itself adds exactly the three expected artifacts.
git -C "$repo" switch -q -C stale "$implementation"
mkdir -p "$repo/$bundle"
printf 'stale\n' > "$repo/$bundle/extra.txt"
git -C "$repo" add "$bundle/extra.txt"
git -C "$repo" commit -qm "stale receipt content"
printf '{}\n' > "$repo/$bundle/bundle.json"
printf '{}\n' > "$repo/$bundle/result.json"
printf 'patch\n' > "$repo/$bundle/result.patch"
git -C "$repo" add "$bundle"
git -C "$repo" commit -qm "receipt with stale envelope content"
if "$binder" --repo "$repo" --bundle "$bundle" --output "$root/stale" > /dev/null 2>&1; then
  echo "binder accepted stale receipt content" >&2
  exit 1
fi

# Path traversal, a dirty checkout, and a dangling output symlink are rejected
# before any worktree appears.
if "$binder" --repo "$repo" --bundle '../escape' --output "$root/escape" > /dev/null 2>&1; then
  echo "binder accepted path traversal" >&2
  exit 1
fi
ln -s missing-target "$root/dangling"
if "$binder" --repo "$repo" --bundle "$bundle" --output "$root/dangling" > /dev/null 2>&1; then
  echo "binder accepted a dangling output symlink" >&2
  exit 1
fi
printf 'dirty\n' >> "$repo/app.txt"
if "$binder" --repo "$repo" --bundle "$bundle" --output "$root/dirty" > /dev/null 2>&1; then
  echo "binder accepted a dirty checkout" >&2
  exit 1
fi

printf 'committed receipt binder tests passed\n'
