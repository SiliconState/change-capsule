# Changelog

## 0.1.0 (unreleased)

First public release: agent-neutral, isolated code-change attempts backed by
ordinary Git worktrees, sealed into portable, verifiable receipts.

- Isolated capsules pinned to an exact base commit; parallel attempts never
  race on one checkout and never touch the primary worktree.
- Complete binary-capable patch, changed-path inventory, checkpoints,
  caller-recorded evidence, and content digests sealed at close.
- `capsule export` produces a self-describing receipt (`bundle.json`,
  `result.json`, `result.patch`); `capsule verify` re-checks it anywhere with
  no capsule state, and `--repo` proves the sealed patch reproduces exactly
  against the pinned base.
- Journaled, crash-recoverable create/checkpoint/integrate/drop transitions;
  `capsule recover` completes only provable transitions.
- Drift detection covers tracked content; the ignored-content inventory is
  sealed at close as provenance and its later churn (build output, caches)
  does not block integration or cleanup.
- Repository allowlists and resource limits enforced at mutation boundaries;
  usage that no configured limit references is never measured.
- Evidence is bounded (64 records, 256 KiB total) so the durable manifest
  cannot outgrow its own storage cap.
- GitHub Action merge gate at the repository root: verifies a receipt in CI and
  can refuse a merge unless the tree equals the pinned base plus the sealed
  patch. Runs locally too, via `scripts/verify-gate.sh`.
- Runnable demo: `examples/parallel-attempts.sh`.
