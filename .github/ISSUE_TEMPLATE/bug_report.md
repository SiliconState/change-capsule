---
name: Bug report
about: Something behaves differently from what the documentation states
labels: bug
---

**What happened**

**What you expected**

Quote the documentation that led you to expect it, if you can.

**Reproduction**

```sh
capsule --json ...
```

**Environment**

- `capsule --version`:
- OS and version:
- Git version:

**Receipt** (if relevant)

Attach `bundle.json` and `result.json`. They are designed to travel and carry no
host paths. Do not paste an idempotency key: it is opaque local state, and while
it is not a credential, it may embed your run identifiers.

> Not a bug: evidence is caller-asserted, the workspace is not a sandbox, and the
> threat model assumes one trusted local user. See [SECURITY.md](../../SECURITY.md).
