# Standards Interoperability

**Short version:** if you already run in-toto, SLSA, or Sigstore, you do not
have to adopt a second format. `capsule attest` emits a standard
[in-toto Statement v1] carrying a Capsule predicate, and `cosign attest-blob`
signs it with the identity infrastructure you already have.

You also do not have to take Capsule's word for anything: the statement is a
pure function of the receipt, so any holder can regenerate it and compare.

## Contents

- [Why a receipt and not just an attestation](#why-a-receipt-and-not-just-an-attestation)
- [What the receipt proves](#what-the-receipt-proves)
- [Emitting an in-toto Statement](#emitting-an-in-toto-statement)
- [Signing with Sigstore](#signing-with-sigstore)
- [Field mapping](#field-mapping)
- [Why exact bytes instead of canonicalisation](#why-exact-bytes-instead-of-canonicalisation)
- [Where this sits in SLSA terms](#where-this-sits-in-slsa-terms)

## Why a receipt and not just an attestation

This is the first question a security team asks, and it deserves a direct
answer rather than a defence of a private schema.

in-toto and SLSA describe **how an artifact was built**. They bind a producer,
a build definition, and an output digest, and a verifier checks *signatures and
*signatures and policy* over those claims. That is the right model for a compiled artifact,
where nobody can re-derive the output from the inputs cheaply.

A source change is different: the output **can** be re-derived, exactly and
cheaply. So Capsule's receipt carries the patch itself alongside its pinned
base, which lets `capsule verify --repo` do something an attestation cannot:

```text
apply(sealed patch, pinned base) == the tree being merged   # byte for byte
```

That is a *reproduction* check, not a trust check. It holds even if every
signature is absent, every key is unknown, and the producing machine is assumed
hostile. An in-toto attestation over the same change would tell you a signer
*claimed* the digest; the receipt lets you recompute it.

So the two are complementary, not competing:

| | Establishes | Needs to trust |
| --- | --- | --- |
| Capsule receipt | The patch reproduces this exact tree from this exact base | Nothing |
| Signature over the receipt | *Who* produced it | The verifier's out-of-band key |
| in-toto / SLSA envelope | Fits your existing policy engine and transparency log | Your existing PKI |

Use the receipt for correctness, and the attestation to carry it through the
policy and identity tooling you already run.

## What the receipt proves

Stated plainly, because "verifiable AI code" claims more than anything can
deliver. The same list is machine-readable inside every statement, under
`predicate.proof_boundary`.

**Proves** — recomputable by anyone holding the receipt:

- the patch bytes match their sealed SHA-256 and byte count;
- the patch applies to the pinned base commit;
- applying it reproduces exactly the sealed bytes and changed-path inventory;
- the sealed result is internally consistent;
- with `--verify-head`, the tree being merged **is** base plus that patch;
- with a signature, a specific out-of-band key signed those exact bundle bytes.

**Does not prove** — and no amount of signing changes this:

- that a record with `executed: false` ever ran, or that its reported output is
  honest — that one is a caller's assertion and nothing more. A record with
  `executed: true` did run, because Capsule spawned it and watched it, but even
  then you are trusting the host that ran Capsule;
- who or what wrote the change — a human, an agent, or neither;
- that the change is correct, safe, reviewed, or good;
- that the producing host was uncompromised.

The practical consequence: a merge gate built on this proves **the diff being
merged is the diff that was reviewed and sealed**. It is an integrity control,
not a quality control. Keep running your own tests in CI; the gate tells you
*what* you are testing, not that someone else already did.

## Emitting an in-toto Statement

```sh
capsule attest ./handoff --repo .
```

`attest` verifies the receipt first and refuses to emit anything for a receipt
that does not verify, so a statement can never launder a tampered bundle.
Passing `--repo` additionally runs the reproduction check before emission. The
statement is identical either way: `proof_boundary` describes what any verifier
*can* re-derive from the receipt, not which checks one particular emission ran —
a consumer trusts the statement by regenerating it, never by trusting whoever
emitted it.

```json
{
  "_type": "https://in-toto.io/Statement/v1",
  "subject": [
    { "name": "result.patch", "digest": { "sha256": "..." } },
    { "name": "head", "digest": { "gitCommit": "..." } }
  ],
  "predicateType": "https://github.com/SiliconState/change-capsule/attestation/change/v1",
  "predicate": { "...": "..." }
}
```

Both subject digests use standard in-toto algorithm names; `gitCommit` is the
lowercase hex object id, exactly as Capsule stores commits. The document is
deterministic — no timestamps beyond those already sealed, no host paths, no
map iteration order — so regenerating it from the same receipt is byte-identical.

The statement is deliberately **not** added to the receipt. `bundle.json`
describes exactly two artifacts — `result.json` and `result.patch` — and the
gate checks that three-file envelope exactly; an extra file would
break every existing verifier for no gain. Generate the statement when you need
it: it is a pure function of the receipt, so nothing is lost by not storing it.

## Signing with Sigstore

`cosign attest-blob` builds the Statement itself and derives the subject from
the blob, so hand it the bare predicate and the patch:

```sh
capsule attest ./handoff --repo . --predicate-only > predicate.json

cosign attest-blob ./handoff/result.patch \
  --predicate predicate.json \
  --type https://github.com/SiliconState/change-capsule/attestation/change/v1 \
  --bundle change.sigstore.json
```

Verify with the identity you expect, keylessly:

```sh
cosign verify-blob-attestation ./handoff/result.patch \
  --bundle change.sigstore.json \
  --type https://github.com/SiliconState/change-capsule/attestation/change/v1 \
  --certificate-identity-regexp '^https://github.com/your-org/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

That gives you a transparency-log entry and workload identity without Capsule
managing any keys. Capsule's own `capsule sign` remains available for air-gapped
or Sigstore-free environments; it signs the exact `bundle.json` bytes with a raw
Ed25519 key you supply out of band.

## Field mapping

| Capsule receipt (`result.json`) | in-toto Statement |
| --- | --- |
| `patch_sha256` | `subject[].digest.sha256` |
| `head_commit` | `subject[].digest.gitCommit` |
| `base_commit` | `predicate.base_commit` |
| `changed_paths` | `predicate.changed_paths` |
| `kind` | `predicate.kind` |
| `ignored_content_sha256` | `predicate.ignored_content_sha256` |
| `evidence[]` | `predicate.evidence[]`, with `executed` and `current_for_sealed_patch` |
| `label`, `links` | `predicate.label`, `predicate.links` |
| — | `predicate.proof_boundary` (extension; see above) |

Idempotency keys and their request metadata are **never** included. They are
local orchestration state; a receipt proves result consistency, not who ran what.

## Why exact bytes instead of canonicalisation

Capsule's own signature covers a fixed domain-separated SHA-256 of the exact
`bundle.json` bytes on disk. Sigstore's DSSE envelope and JCS-based schemes
instead canonicalise a parsed object before signing. Both are defensible; the
exact-bytes choice is deliberate:

- **No parser gap.** Canonicalisation signs the result of *your* parse. Signer
  and verifier must agree on number formatting, Unicode normalisation, duplicate
  key handling, and key ordering. Every disagreement is a signature-bypass
  candidate. Hashing the bytes removes the class entirely.
- **Verification reads the file once.** `verify_authenticated_bundle` takes one
  in-memory snapshot and applies both the signature check and receipt
  verification to it, so nothing can change between the two.
- **The bundle is already content-addressed.** `bundle.json` binds
  `result.json` and `result.patch` by digest and byte count, so signing the
  bundle bytes transitively covers all three artifacts.

The cost is that a byte-identical re-serialisation is required to re-verify — you
must keep the bundle you signed, not a re-encoded copy. For an artifact designed
to be copied verbatim, that is the correct trade. When you want DSSE and
transparency-log semantics instead, use the Sigstore path above; the two coexist.

## Where this sits in SLSA terms

Capsule attests to a **source change**, not a build. In SLSA vocabulary it sits
upstream of the build track: it establishes that a specific diff, from a
specific base, is what entered your repository, and it is enforceable at the
merge gate rather than at deploy time.

It composes cleanly with build provenance. A typical pipeline ends up with:

1. a Capsule receipt proving *this diff is what was merged*;
2. SLSA build provenance proving *this artifact was built from that commit*.

Neither subsumes the other, and the gap between them — source to commit — is
exactly the one Capsule closes.

[in-toto Statement v1]: https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md
