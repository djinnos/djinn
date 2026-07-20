# Memory-sustainability operator evidence checklist

Copy this checklist into the change record. A blank item is not approval. Attach immutable paths and checksums rather than copying sensitive tokens or mutable dashboards.

## Identity and scope

- [ ] Change/release ID: `____________________`
- [ ] Cluster / namespace / Helm release: `____________________ / ____________________ / ____________________`
- [ ] Candidate image digest (immutable `@sha256:`): `____________________`
- [ ] Pre-change diagnostic image digest (immutable `@sha256:`): `____________________`
- [ ] Candidate run identity: `____________________`
- [ ] Pre-change diagnostic run identity: `____________________`
- [ ] Evidence root (durable path): `____________________`
- [ ] Candidate start and finish timestamps (UTC): `____________________ / ____________________`
- [ ] Diagnostic start and finish timestamps (UTC): `____________________ / ____________________`

## Fixture, target, and baseline proof

- [ ] Fixture profile/seed: `production / ste6-production-v1`
- [ ] Fixture directory and fixture-report path: `____________________`
- [ ] Fixture manifest SHA-256: `____________________`
- [ ] Fixture report SHA-256: `____________________`
- [ ] Canonical graph / board / artifact checksums recorded from fixture report: `____________________`
- [ ] Cgroup identity (pod/container/cgroup path): `____________________`
- [ ] Cgroup memory.max is exactly `4294967296`: `____________________`
- [ ] T0 timestamp confirms 30 minute idle and no graph slot/generation: `____________________`
- [ ] Graph generation identity after install: `____________________`
- [ ] Same graph generation identity is recorded through T1, burst, and T2: `____________________`
- [ ] Restart baseline and final/delta: `____________________ / ____________________`
- [ ] OOM/oom_kill baseline and final/delta: `____________________ / ____________________`

## Required protocol evidence

- [ ] Preflight proves fixture identity, cgroup signals, metrics, 40-page board pass, and 200/304 route pair.
- [ ] T0 raw sample path/record ID: `____________________`
- [ ] Graph-install peak path/record ID (server and warm): `____________________`
- [ ] T1 raw sample path/record ID: `____________________`
- [ ] Burst raw evidence path; 2 hours, five-minute ticks, 100 sequential alternating 200/304 requests: `____________________`
- [ ] T2 raw sample path/record ID: `____________________`
- [ ] Candidate raw JSONL path and SHA-256: `____________________ / ____________________`
- [ ] Diagnostic raw JSONL path and SHA-256: `____________________ / ____________________`
- [ ] Driver `.partial` paths retained if any failure/interruption occurred: `____________________`
- [ ] Evaluator input path and SHA-256: `____________________ / ____________________`
- [ ] Machine-readable `evaluation.json` path and SHA-256: `____________________ / ____________________`
- [ ] Human `evaluation.md` path and SHA-256: `____________________ / ____________________`
- [ ] Candidate evaluator result is `pass` (all checks): `____________________`
- [ ] Pre-change diagnostic result recorded as non-gating comparison: `____________________`

## Review, rollout, and rollback decision

- [ ] Reviewer checked raw paths, image/fixture/cgroup/generation identities, OOM/restart baselines, and evaluator result.
- [ ] Reviewer name and timestamp: `____________________ / ____________________`
- [ ] Release approver name and timestamp: `____________________ / ____________________`
- [ ] Rollout decision: `[ ] advance  [ ] hold  [ ] rollback`
- [ ] Rollback decision, step, operator, and timestamp (or `not invoked`): `____________________`
- [ ] Expand -> dual-read server -> warmer -> UI order recorded; each rollback remains available.
- [ ] Helm retention dry-run evidence path and reviewer decision: `____________________`
- [ ] Helm retention delete evidence path and reviewer decision (only after dry-run): `____________________`
- [ ] Legacy telemetry observation path/count: `____________________`
- [ ] Rollback-window close timestamp: `____________________`
- [ ] Contract/drop **not performed by this epic**. It remains prohibited until zero legacy telemetry is observed and the rollback window is closed.
