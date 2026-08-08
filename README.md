# Capsule

[![crates.io](https://img.shields.io/crates/v/change-capsule)](https://crates.io/crates/change-capsule)
[![docs.rs](https://img.shields.io/docsrs/change-capsule)](https://docs.rs/change-capsule)
[![CI](https://github.com/SiliconState/change-capsule/actions/workflows/ci.yml/badge.svg)](https://github.com/SiliconState/change-capsule/actions/workflows/ci.yml)

**An agent wrote a patch and says the tests pass. Prove it — on a different machine, with no access to the one that made it.**

Capsule gives an agent an isolated Git worktree to work in, runs the
verification command itself, and seals the whole attempt into a directory of
three files. Anyone with that directory can re-derive the result from scratch.

```sh
capsule create --repo .                    # isolated worktree, pinned base
capsule evidence <id> -- cargo test        # Capsule runs this and watches
capsule close <id> --require-executed-evidence
capsule export <id> --output ./receipt     # bundle.json, result.json, result.patch
capsule verify ./receipt --repo . --require-executed-evidence
```

## Why not just push a branch?

Because a commit SHA proves the tree exists, not that anything checked it, and
it only works if the producer and the reviewer share a remote.

The deeper reason is that **a source change can be re-derived and a build
artifact cannot.** in-toto and SLSA exist because nobody can cheaply recompute a
compiled binary from its inputs, so a verifier checks *signatures* over a
producer's claims instead. A patch is different. Apply it to the pinned base and
you get the tree back, byte for byte:

```text
apply(sealed patch, pinned base) == the tree being merged
```

That is a **reproduction** check, not a trust check. It holds when every
signature is absent, every key is unknown, and the producing machine is assumed
hostile. So Capsule carries the patch itself and lets you recompute, rather than
asking you to believe a digest.

The second half is evidence. `capsule evidence <id> -- cargo test` spawns the
program directly in the capsule workspace and records the exit code and a digest
of the output it observed. The receipt marks that record `executed: true`. A
verifier can then demand it:

| | Establishes | You must trust |
| --- | --- | --- |
| The patch reproduces the tree | Recomputed by anyone | Nothing |
| Executed evidence passed | The command ran and passed | The producing host |
| A caller's claim passed | Someone said so | The producer, entirely |
| A signature over the receipt | *Who* produced it | Your out-of-band key |

Capsule never blurs those rows. Evidence it did not run is stored as
`executed: false` and can never satisfy `--require-executed-evidence`.

## Where this earns its keep

**Evaluation harnesses.** Run N candidate patches from one pinned base. Each
gets its own worktree and index, so candidates never race on a shared checkout.
Each seals into a comparable, independently verifiable result. Attach dimensions
with `--link eval=parser-v2 --link candidate=model-a --link sample=184`.

**Handoff across a trust boundary.** A contractor, a partner team, an
autonomous agent, or an air-gapped run produces a receipt. You verify it without
their branch, their remote, their state directory, or their word.

If your agent just pushes a branch to a repository you already control, Git
already does most of this. Use Capsule when the producer and the verifier do not
share trust or infrastructure.

## Install

Needs Git on `PATH`, and Rust 1.85+ to build from source.

```sh
cargo install change-capsule
```

The binary is `capsule`. The crate is `change-capsule` because `capsule` was
taken. For library use without the CLI:

```toml
change-capsule = { version = "0.3", default-features = false }
```

## Use it from an agent

The integration surface is small on purpose:

1. call the CLI with `--json`, or embed the Rust crate;
2. give the returned `workspace_path` to the agent as its working directory;
3. let the agent use its own file, search, shell, and Git tools;
4. have Capsule run the verification command;
5. close, export, verify, integrate.

There are no per-agent adapters, and none are needed: anything that can spawn a
process and read JSON can drive the whole loop.

## Walkthrough

[`examples/parallel-attempts.sh`](examples/parallel-attempts.sh) runs the
complete story end to end: two competing attempts from one base, executed
evidence, sealed receipts, tamper detection, explicit integration, cleanup.

```text
pinned Git commit
      │
      ├── capsule A ── ordinary worktree ── sealed patch + provenance
      ├── capsule B ── ordinary worktree ── sealed patch + provenance
      └── primary worktree untouched until explicit integration
```

## Merge gate

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0          # the pinned base must be present
- uses: SiliconState/change-capsule@v0.3.0
  with:
    bundle: ./receipt
    repo: .
    require-executed-evidence: "true"
    verify-head: "true"
```

The gate passes only when all of this holds:

1. the bundle's artifacts match their descriptor digests and byte counts;
2. the sealed result is internally consistent and its schema is supported;
3. the pinned base exists in the checkout, and the sealed patch applies to it,
   reproducing exactly the sealed bytes and changed paths;
4. Capsule itself ran a command that passed against that exact patch;
5. the checked-out tree equals base plus the sealed patch.

Run the same checks locally with
`scripts/verify-gate.sh --bundle ./receipt --repo . --require-executed-evidence`.

Capsule does not commit receipts into the repository, and does not ask you to
change how you merge.

## What a receipt proves

Stated plainly, because "verifiable AI code" claims more than anything can
deliver. Every attestation carries this same list machine-readably under
`predicate.proof_boundary`.

**Proves**, recomputable by anyone holding the receipt, trusting nothing:

- the patch bytes match their sealed digest and byte count;
- the patch applies to the pinned base and reproduces exactly the sealed bytes
  and changed paths;
- with `--verify-head`, the tree being merged **is** base plus that patch;
- with a signature, a specific out-of-band key signed those exact bundle bytes.

**Also proves**, if the receipt carries executed evidence, but only as far as
you trust the machine that produced it:

- the recorded command ran in the capsule workspace, and Capsule observed that
  exit code and that output digest.

**Does not prove**, and signing does not change this:

- that a `executed: false` record ever ran — that is a caller's assertion;
- who or what wrote the change;
- that the change is correct, safe, reviewed, or good;
- that the producing host was uncompromised.

So a merge gate on this proves **the diff being merged is the diff that was
sealed, and it passed a command Capsule ran**. That is an integrity control, not
a quality control. Keep running your own tests. If you already use in-toto,
SLSA, or Sigstore, `capsule attest` emits a standard in-toto Statement — see
[`docs/interop.md`](docs/interop.md).

## CLI

```text
capsule create       create an isolated attempt from a resolved base commit
capsule list         list durable capsule records
capsule show         show the full manifest
capsule path         print the workspace path
capsule status       inspect health, changed paths, commits, and seal state
capsule diff         emit the complete current or sealed patch
capsule checkpoint   commit current work with an explicit identity
capsule evidence     run a verification command, or record a caller's claim
capsule close        seal patch, inventory, evidence, and digest
capsule result       show the sealed handoff manifest
capsule artifacts    discover sealed artifacts, URIs, sizes, and content addresses
capsule export       write a portable receipt directory
capsule verify       verify a receipt offline, optionally against a repository
capsule attest       emit an in-toto Statement for a verified receipt
capsule keygen       generate matching raw Ed25519 key files
capsule sign         create a detached Ed25519 signature over bundle.json
capsule integrate    apply one sealed result to its pinned base
capsule drop         safely remove an owned worktree and branch
capsule recover      reconcile interrupted journals
capsule capabilities print the static machine-readable protocol contract
capsule lookup       resolve one idempotency key without scanning state
```

`--json` is global. In JSON mode, errors are one object on stderr carrying a
stable `kind`, so no consumer parses error text. State lives in the platform
state directory, overridable with `CAPSULE_HOME` or `--home`.

### Evidence, in both forms

```sh
# Executed: Capsule spawns the program. No shell, so no quoting surprises.
capsule evidence <id> --timeout-seconds 600 -- cargo test --all-features

# Claimed: for a run Capsule could not perform, such as one on other hardware.
# This can never satisfy --require-executed-evidence.
capsule evidence <id> --claim "cargo test on the GPU runner" --exit-code 0
```

`close` and `verify` accept three independent evidence requirements, not a
ladder. `--require-executed-evidence` is the one to use: it asks for a record
Capsule ran itself, that passed, bound to the patch being sealed.
`--require-successful-evidence` asks something quite different — that *every*
record passed — so it rejects an attempt whose tests failed once and were then
fixed. That is rarely what you want, and it is off by default in the Action.

### Crash-safe creation

An orchestrator that can time out or restart should pass an idempotency key.
The same key always resolves to the same attempt, and `lookup` answers "did my
earlier call already create this?" without scanning a large state root:

```sh
capsule create --repo . --idempotency-key "run:8f21/attempt:1"
capsule lookup --idempotency-key "run:8f21/attempt:1"
```

Keys are opaque local orchestration state, **not credentials**. Do not put
secrets in them. They never appear in a receipt.

### Signing

Optional authenticity over the exact exported `bundle.json` bytes, with a raw
Ed25519 keypair. The verifier supplies the trusted key out of band; no key
inside a receipt is ever trusted:

```sh
capsule keygen --private-key ./ed25519.seed --public-key ./ed25519.pub
capsule sign ./receipt --private-key ./ed25519.seed --output ./receipt.sig
capsule verify ./receipt --signature ./receipt.sig --trusted-public-key ./ed25519.pub
```

## Rust API

```rust
use change_capsule::{
    CapsuleManager, CloseOptions, CreateOptions, EvidenceInput, VerifyOptions, verify_bundle,
};

let manager = CapsuleManager::open_default()?;
let capsule = manager.create(CreateOptions::new(".").with_label("candidate a"))?;

// Launch any external tool with capsule.workspace_path as its cwd. Then have
// Capsule run the verification command, so the result is observed, not asserted.
manager.add_evidence(&capsule.id, EvidenceInput::run(["cargo", "test"]))?;

// Sealing refuses to proceed unless that command passed on this exact patch.
let result = manager.close(&capsule.id, CloseOptions::executed())?;
manager.export_artifacts(&capsule.id, "./receipt")?;

// Anywhere else, holding only ./receipt and a clone of the repository:
let report = verify_bundle("./receipt", &VerifyOptions::strict("."))?;
assert_eq!(report.patch_sha256, result.patch_sha256);
```

The crate owns the attempt lifecycle, provenance, artifacts, and receipts. The
caller owns process launch, model choice, prompts, credentials, and sandboxing.
The workspace isolates Git state; it is not a security sandbox. Run untrusted
code under an external sandbox.

## Guarantees

1. Many attempts may start at the same immutable commit, each with its own
   worktree and index, and may change the same files without interfering.
2. The source worktree stays untouched until explicit integration.
3. Every result carries a complete patch, changed-path inventory, digest, and
   sealed provenance, and its receipt verifies offline on any machine.
4. Evidence records whether Capsule ran the command. That flag cannot be forged
   through the API, and a verifier can require it.
5. Journaled checkpoint, integration, and cleanup transitions survive process
   restart and can be recovered by another process.
6. Missing, replaced, drifted, or unrepresentable workspaces fail closed.
7. Cleanup refuses foreign directories even with `--force`.
8. Integration is explicit and requires a clean target at the exact pinned base.

## Scope

Capsule owns an attempt boundary. It does not plan work or orchestrate agents,
and it composes with the tools that do.

Within Git repositories: capsule workspaces disable inherited sparse checkout; a
private temporary index makes `skip-worktree` and `assume-unchanged` irrelevant
to snapshots; dirty submodules and unregistered embedded repositories are
rejected rather than silently omitted; non-UTF-8 inventory paths use a canonical
`{ "unix_bytes_hex": ... }` form; ignored untracked content is excluded from the
patch but recorded as provenance after two matching close-time inventories.

Out of scope: remote execution, distributed persistence, background jobs,
non-Git snapshots, automatic rebasing, merge queues, network services, resource
quotas, and execution sandboxing.

## Status

Durable state and receipts are schema v5. Earlier schemas are not read; this
crate is young enough that carrying migration machinery costs more than it
saves.

- [`docs/architecture.md`](docs/architecture.md) — components, state, lifecycle
- [`docs/protocol.md`](docs/protocol.md) — the contract for agents and automation
- [`docs/security.md`](docs/security.md) — trust assumptions and known limits
- [`docs/composition.md`](docs/composition.md) — composing with agents and CI
- [`docs/interop.md`](docs/interop.md) — in-toto, SLSA, and Sigstore

## License

MIT.
