#!/usr/bin/env bash
# Two isolated attempts at one task, sealed into verifiable receipts.
#
# The demo needs only bash, git, and this crate. It:
#   1. creates a scratch repository with a failing script;
#   2. runs two competing "agents" (deterministic edits) in isolated capsules;
#   3. records real verification evidence and seals both results;
#   4. exports the winning result and verifies the receipt offline;
#   5. shows that a tampered receipt is rejected;
#   6. integrates the winner explicitly and cleans up, keeping both receipts.
#
# Override CAPSULE_BIN to use an installed binary instead of cargo run.
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
manifest="$script_dir/../Cargo.toml"

if [[ -n "${CAPSULE_BIN:-}" ]]; then
  capsule() { "$CAPSULE_BIN" "$@"; }
else
  capsule() { cargo run --quiet --manifest-path "$manifest" -- "$@"; }
fi

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
state="$workdir/state"
repo="$workdir/project"

cap() { capsule --home "$state" "$@"; }

step() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

step "Seed a repository whose test fails"
mkdir "$repo"
git -C "$repo" init -q -b main
cat > "$repo/greet.sh" <<'EOF'
#!/bin/sh
echo "Hallo, World!"
EOF
cat > "$repo/test.sh" <<'EOF'
#!/bin/sh
[ "$(sh "$(dirname "$0")/greet.sh")" = "Hello, World!" ]
EOF
chmod +x "$repo/greet.sh" "$repo/test.sh"
git -C "$repo" add .
git -C "$repo" -c user.name=Demo -c user.email=demo@example.test commit -qm "seed failing greeting"
sh "$repo/test.sh" && { echo "expected the seeded test to fail"; exit 1; }
echo "test fails at the pinned base, as intended"

create_capsule() {
  # Prints "id<TAB>workspace" for a fresh capsule labeled $1.
  local out
  out=$(cap create --repo "$repo" --label "$1" --link task=demo-42)
  printf '%s\t%s\n' \
    "$(printf '%s\n' "$out" | head -1)" \
    "$(printf '%s\n' "$out" | sed -n 's/^path=//p')"
}

step "Attempt A: minimal fix, in its own capsule"
IFS=$'\t' read -r id_a ws_a < <(create_capsule "approach A: minimal fix")
sed -i.bak 's/Hallo/Hello/' "$ws_a/greet.sh" && rm -f "$ws_a/greet.sh.bak"
if sh "$ws_a/test.sh"; then status_a=0; else status_a=$?; fi
cap evidence "$id_a" --command "sh test.sh" --exit-code "$status_a" --summary "greeting test" > /dev/null
echo "capsule $id_a: test exit code $status_a"

step "Attempt B: rewrite, in a parallel capsule from the same base"
IFS=$'\t' read -r id_b ws_b < <(create_capsule "approach B: rewrite")
cat > "$ws_b/greet.sh" <<'EOF'
#!/bin/sh
printf '%s\n' "Hello, World!"
EOF
if sh "$ws_b/test.sh"; then status_b=0; else status_b=$?; fi
cap evidence "$id_b" --command "sh test.sh" --exit-code "$status_b" --summary "greeting test" > /dev/null
echo "capsule $id_b: test exit code $status_b"
echo "primary worktree untouched: $(cat "$repo/greet.sh" | tail -1)"

step "Seal both attempts; sealing refuses missing or failed evidence"
cap close "$id_a" --require-successful-evidence > /dev/null
cap close "$id_b" --require-successful-evidence > /dev/null
echo "both results sealed with evidence"

step "Export the selected result as a portable receipt"
bundle="$workdir/receipt-a"
cap export "$id_a" --output "$bundle" > /dev/null
ls -1 "$bundle"

step "Verify the receipt offline, then against the repository"
cap verify "$bundle" --require-successful-evidence > /dev/null
echo "offline verification passed"
cap verify "$bundle" --repo "$repo" --require-successful-evidence > /dev/null
echo "patch reproduces exactly against the pinned base"

step "A tampered receipt is rejected"
tampered="$workdir/receipt-tampered"
cp -R "$bundle" "$tampered"
printf 'x' >> "$tampered/result.patch"
if cap verify "$tampered" > /dev/null 2>&1; then
  echo "tampering was NOT detected"; exit 1
fi
echo "tampering detected and rejected"

step "Integrate the winner explicitly; the loser stays reviewable"
cap integrate "$id_a" --target "$repo" -m "select approach A" > /dev/null
git -C "$repo" log --oneline -1
sh "$repo/test.sh" && echo "test now passes on main"

step "Cleanup removes workspaces, never receipts"
cap drop "$id_a" > /dev/null
cap drop "$id_b" --force > /dev/null
cap verify "$bundle" > /dev/null && echo "exported receipt still verifies after drop"
cap result "$id_b" > /dev/null && echo "unchosen attempt's sealed result is still readable"

printf '\n\033[1mDone.\033[0m Two agents, one repo, zero races — and receipts you can check anywhere.\n'
