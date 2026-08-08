# Composition

Capsule is universal infrastructure at the attempt boundary. It is designed to sit below many different tools without knowing their concepts.

## Contents

- [Negotiating and retrying](#negotiating-and-retrying)
- [Coding agents](#coding-agents)
- [CI and evaluation harnesses](#ci-and-evaluation-harnesses)
- [Task trackers](#task-trackers)
- [Workflow and multi-agent systems](#workflow-and-multi-agent-systems)
- [Editors and humans](#editors-and-humans)
- [Rust embedding](#rust-embedding)
- [Shell composition](#shell-composition)
- [Non-goals for adapters](#non-goals-for-adapters)

## Negotiating and retrying

Two surfaces exist specifically for the systems in this document — coordinators, CI and evaluation harnesses, task runners, and multi-agent systems — rather than for a single attempt.

`capsule --json capabilities` is a static compatibility probe an integration can run before anything else. It never touches `CAPSULE_HOME` or Git and succeeds against a missing, unwritable, malformed, or incompatible state root, so it is safe to call against an unknown installation. Branch on a supported protocol version and the feature identifiers you actually use, not on the package version.

`capsule --json create --idempotency-key <key>` plus `capsule --json lookup --idempotency-key <key>` is the crash-safe creation path. A coordinator that can time out or restart should derive a key from its own run/attempt identity, retry the identical create, and use lookup — not `list` — to answer "did my earlier attempt already create this?". Keys are scoped to one state root, opaque, and not credentials; do not put secrets in them.

This removes the full-state discovery scan from crash recovery, which matters most for exactly the large multi-agent state roots described below. It does not mean the worker ran once: a replay can return a capsule already closed, integrated, orphaned, or dropped, so a coordinator still checks `status` and uses targeted `recover <id>`.

## Coding agents

For any coding agent that accepts a working directory:

```text
capsule create --json
       │
       ├── workspace_path ──▶ launch agent with cwd=workspace_path
       │
       └── capsule id ──────▶ retain for status, close, review, and cleanup
```

The agent continues to use its own file tools, shell, Git behavior, prompts, permissions, and UI. No plugin is required for basic use.

An interactive agent can be launched manually from the path printed by `capsule path`. A headless runner can parse the create response and set `cwd` directly.

## CI and evaluation harnesses

A CI job can create one capsule per candidate implementation, have Capsule run each candidate's tests inside its own workspace, close all results, and compare their patches without allowing candidates to race on one checkout. Because the runs are executed rather than reported, a candidate cannot be recorded as passing unless it did.

A merge gate can require a receipt: the agent-side harness runs `capsule export`, attaches the bundle to the change, and CI runs `capsule verify <bundle> --repo . --require-executed-evidence` plus a byte comparison of `result.patch` against the proposed diff. That proves the sealed patch reproduces the tree and that a command Capsule ran itself passed against exactly that patch. It does not prove the producing host was honest, so CI should still rerun security-critical tests on its own hardware.

Attach the receipt to the change however your review tool already carries artifacts: a CI artifact, a release asset, or a directory the gate fetches by capsule ID. Capsule deliberately does not ask you to commit receipts into the repository, because a tree cannot contain a receipt that describes itself, and every workaround for that taxes the commit graph.

An evaluation harness can attach dimensions using links:

```sh
--link eval=patch-generation-v2
--link candidate=model-a
--link sample=184
```

The result patch, manifest, artifact descriptors, exported bundle, and evidence remain stable after the worktree is dropped. Harnesses may implement `ArtifactSink` to stream sealed bytes into their own object store or CAS without adding that backend to core.

## Task trackers

A tracker answers what work exists. Capsule answers where one attempt happened and what it produced.

Attach the tracker ID as opaque metadata:

```sh
capsule create --repo . --link task=tracker-42 --link tracker=custom
```

The tracker may store the returned capsule ID in its own metadata. Neither system needs a dependency on the other.

Beads is one example of such a tracker. Its existing `bd worktree` helper overlaps with raw worktree creation, but Beads' published charter keeps execution attempts and orchestration outside issue-tracking core. A Beads integration could claim a bead, create one or more linked capsules, and attach the selected result ID. This is optional composition, not the product target or a core dependency.

The same pattern applies to GitHub Issues, Linear, Jira, plain files, or an in-house queue.

## Workflow and multi-agent systems

A coordinator can create several capsules from one base:

```text
task
├── capsule candidate-a
├── capsule candidate-b
└── capsule candidate-c
```

Workers may modify identical paths concurrently because their files and indexes are isolated. Reviewers consume sealed result manifests, patches, or self-describing exports. The coordinator chooses if and when to integrate. Every worker reports through the same crate and CLI, with no coordinator-specific adapter.

Capsule intentionally does not choose workers, route tasks, retry models, resolve conflicts, or select winners. Idempotent creation is the one concession to coordinator failure modes, and it is deliberately narrow: it binds a caller-supplied key to one capsule identity. It does not add a workflow database, generic task/agent/run tables, arbitrary link queries, a daemon, a background index, key expiration, automatic retries, or winner selection.

## Editors and humans

The returned workspace is an ordinary directory. It can be opened in an editor, terminal, language server, debugger, or file browser. There is no FUSE mount or virtual API to support.

## Rust embedding

The reusable API is `CapsuleManager`. Embedders can:

- choose an explicit state root;
- read the static protocol contract with `Capabilities::current()`, without opening a manager;
- create capsules with labels and arbitrary links;
- create idempotently with `create_idempotent` and resolve a key directly with `lookup_idempotency_key`, or with manager-free `lookup_idempotency_key_at`;
- retrieve paths and status;
- record checkpoints, and run or record evidence;
- close and inspect results;
- discover, stream, publish, or export sealed artifacts;
- verify exported v3/v4 bundles offline with `verify_bundle`, or authenticate and verify one exact bundle snapshot with `verify_authenticated_bundle` and an out-of-band trusted key;
- integrate and drop explicitly;
- call recovery at startup.

Disable default features to omit the CLI dependency:

```toml
change-capsule = { version = "0.2", default-features = false }
```

## Shell composition

A minimal framework-neutral runner:

```sh
created=$(capsule --json create --repo . --link task="$TASK_ID")
id=$(printf '%s' "$created" | jq -r .id)
workspace=$(printf '%s' "$created" | jq -r .workspace_path)

(
  cd "$workspace"
  your-agent-command "$TASK_PROMPT"
)
agent_status=$?

# Capsule runs the tests itself, in the capsule workspace, and records the exit
# code and output digest it observed. The harness cannot assert a pass it did
# not get, which is what makes the receipt worth checking later.
capsule --json evidence "$id" --timeout-seconds 900 -- your-test-command

capsule --json close "$id" --require-executed-evidence
```

Production runners should avoid shell parsing by calling the Rust library or decoding JSON in their native language. No cloud, agent, or workflow wrapper is required: any caller can invoke the crate API or `capsule` binary directly.

## Non-goals for adapters

An adapter should not:

- modify capsule state files directly;
- assume branch names or workspace layout;
- write into a closed capsule;
- auto-integrate without an explicit decision;
- claim that a worktree is a security sandbox;
- place credentials in links or evidence;
- make Capsule depend on one agent or tracker.

The stable abstraction is the attempt, not the surrounding product.
