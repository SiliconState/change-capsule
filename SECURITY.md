# Security Policy

## Reporting a vulnerability

Report privately through GitHub's [private vulnerability
reporting](https://github.com/SiliconState/change-capsule/security/advisories/new)
for this repository. Please do not open a public issue for a suspected
vulnerability.

Include the Capsule version (`capsule --version`), operating system, and the
smallest reproduction you can manage. If the issue involves a receipt, attach
the `bundle.json` / `result.json` pair rather than describing them — receipts
are designed to travel and contain no host paths.

Expect an acknowledgement within a few days. Fixes ship in a normal release,
with the advisory published once a fixed version is available.

## What counts as a vulnerability

Capsule's security posture is stated in full in
[`docs/security.md`](docs/security.md), including its explicit non-goals. In
short, these **are** in scope:

- a receipt that verifies but does not describe the change it claims to;
- `capsule verify --repo` accepting a patch that does not reproduce the sealed
  bytes and changed paths;
- signature verification accepting a bundle the trusted key did not sign;
- a path that escapes the capsule workspace, or cleanup removing something
  Capsule does not own;
- state handling that follows a symlink, reads a special file, or writes outside
  the state root;
- an idempotency key resolving to a capsule bound to a different request.

These are **not** vulnerabilities, because they are documented properties rather
than defects:

- **Evidence is caller-asserted.** Capsule records the command and exit code a
  caller reports; it never runs them. A false evidence claim is a false claim,
  not a Capsule bug. The receipt proves change *integrity*, not change *quality*.
- **The workspace is not a sandbox.** It isolates Git state, not processes,
  network, or credentials. Run untrusted code under a real sandbox.
- **The threat model assumes one trusted local user.** A same-user process can
  rewrite state; file locks are advisory. Capsule is not a kernel-enforced
  capability system.
- **Capabilities negotiate protocol, not trust.** `capsule capabilities` says
  what a build implements, never that the binary is authentic.
- **Windows state is not claimed owner-private**, as `docs/security.md` states.

If you are unsure which side of that line something falls on, report it
privately and we will work it out.

## Supported versions

The latest published release receives security fixes. Given the pre-1.0 version,
fixes land in a new minor or patch release rather than being backported.
