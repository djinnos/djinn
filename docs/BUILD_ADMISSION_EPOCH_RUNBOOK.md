# Invocation-lease authority operator runbook

Operator diagnostics and recovery for the durable **invocation-lease authority**:
the arming switch and reference cap for the per-invocation cgroup CPU lease.

It is a single durable row (physically `admission_handoff`, singleton
`name = 'build'`) carrying three things:

| field | what it means |
| --- | --- |
| `mode` | `off` / `shadow` / `enforce`. The operator kill switch. Only `enforce` lifts `cpu.max`. |
| `cap` | The reference cap the build-slot FIFO enforces. Adopted by running processes without a restart. |
| `epoch` | A compare-and-swap fence for operator writes. **Not** an acknowledgement protocol. |

This runbook is diagnosis-and-recovery only. It never asks an operator to
hand-edit the durable row with raw SQL; the `epoch` admin commands are the
supported surface. The one exception is §5, which exists precisely because
startup must never re-create the row implicitly.

## What changed in the Kueue cutover (S3b, task `ubne`)

This used to be a **two-authority handoff** between a v0 "emergency" Postgres
ledger and the v1 invocation authority, with a four-phase ring
(`emergency_primary → forward_overlap → invocation_primary → rollback_overlap`),
per-authority acknowledgement epochs, per-generation acknowledgements, and
multi-step forward/reverse orderings that guaranteed at least one authority was
always enforcing.

Proposal `9oga` deleted the v0 authority. Everything above existed to make the
handover between two authorities safe, so all of it was retired together:

- **`epoch advance` and `epoch rollback` no longer exist.** There is no phase to
  advance through and no ordering to reverse. `epoch arm` and
  `epoch kill-switch` are single, epoch-fenced writes.
- **There are no acknowledgements.** An acknowledgement is a field a writer must
  keep current or the reader fails closed. With one authority there is nothing to
  acknowledge, and an acknowledgement with no writer is a field that can only
  ever silently disarm containment. Arming now reads `mode` alone, so **an epoch
  bump can no longer disarm the lease.**
- **`epoch show` no longer prints `phase`, `v0_mode`, `emergency_ack_epoch` or
  `invocation_ack_epoch`.** Those columns are still physically present and are
  dropped by a later migration (`flc5`); nothing reads them.

If you are following an older copy of this runbook and it tells you to run
`epoch advance`, stop: the state it is trying to reach no longer exists, and
`epoch arm --mode enforce --cap N` is the whole of it.

## Operator commands

Run against a coordinator image with a current schema. These are one-shot admin
modes of the server binary; they open the DB, act, print, and exit before any
actor or HTTP listener starts.

```
djinn-server epoch show                              # print the durable authority row
djinn-server epoch seed                              # create the row, DISARMED, when ABSENT (idempotent)
djinn-server epoch arm --mode enforce --cap N        # arm the lease at cap N
djinn-server epoch arm --mode shadow --cap N         # observe only — see the WARNING below
djinn-server epoch set-cap --cap N                   # change the cap, preserving the mode
djinn-server epoch kill-switch                       # urgent disarm; keeps the cap
```

Every mutation is epoch-fenced: it reads the current epoch, writes against it,
and bumps it. A command issued from a stale epoch is refused with the current
epoch in the message — re-run `show` and retry. Under the hood these compose
`InvocationLeaseControl` in
`server/crates/djinn-coordinator/src/invocation_lease_control.rs`.

`--cap` is required the first time the authority is armed and inherited
otherwise. `set-cap` never changes the mode, and `kill-switch` never clears the
cap, so re-arming after an incident needs no new number.

> **WARNING — `shadow` CLAMPS.** Only `enforce` raises `cpu.max`. `shadow` binds
> the invocation, records what enforcement *would* have done, and then leaves the
> leaf pinned at the broker's unleased quota (250m) for the whole command. Arming
> shadow makes every leased build **slower**, not faster. It is an observation
> mode.

## Diagnosis and recovery procedures

### 1. Fail-closed suspect rows and forced exact-UID deletion

A build lease can land in `suspect` when the coordinator cannot confirm a lease
holder's terminal state (`BuildLeaseState::Suspect`, one of the five occupying
states in `server/crates/djinn-db/src/repositories/build_lease.rs`). A suspect row
keeps counting against the cap on purpose: it is fail-closed until the exact pod
is proven gone.

- **Detect:** `epoch show` reports the effective cap; compare against occupancy.
  Suspect rows appear in `build_leases` with `state = 'suspect'` and in the
  `djinn_build_lease_*` occupancy telemetry.
- **Recover:** do not delete the durable row. Force termination of the **exact**
  pod UID through the watchdog termination seam
  (`WatchdogTerminationRequest` handled in
  `server/crates/djinn-agent/src/direct_services.rs` and
  `server/crates/djinn-agent-worker/src/worker_services.rs`). This deletes only
  the pod whose UID matches — it refuses on empty/mismatched task, task-run, or
  pod UID — so a stale row can never reap a live successor.

### 2. Cap zero (drain without disarming)

- **Do NOT** try to set the cap to zero through `epoch set-cap` — the cap is
  validated to `[MIN_ADMISSION_CAP, MAX_ADMISSION_CAP]` (`1..=4096`) and the
  durable CHECK constraint rejects `cap <= 0`.
- To drain, use the lease service's `BuildLeaseService::set_cap(0)`
  (`server/crates/djinn-coordinator/src/build_lease.rs`), which stops granting new
  leases while occupied rows drain naturally. Light (coordinator-free) commands
  stay unaffected. Restore the cap with the same call once drained.

### 3. `set-cap` takes effect without a restart

`epoch set-cap` changes what the RUNNING coordinator enforces. The durable
reference cap is re-read and adopted by
`BuildLeaseService::refresh_epoch_cap()` on the build-lease maintenance tick
(30s) and again on the authority pass (5 min), and a raised cap immediately
drains the FIFO so work refused at the old cap is granted rather than waiting for
the next dispatch attempt. Lowering the cap stops granting but never revokes an
occupying row.

This was not always true. Before the fix the resolved cap was only written to
the enforcing atomic by `recover()`, so a `set-cap` reported success, `epoch
show` read back the new value, and every denial kept quoting the old one until
the pods restarted (production, 2026-07-25: `set-cap --cap 12`, denials still
`occupancy=3 cap=3`). If you see that shape again, the denial's `cap` field —
not `epoch show` — is the number actually in force.

### 4. Occupancy that no workload is using

A build slot is a `build_leases` row. `BuildLeaseReclaimer`
(`server/crates/djinn-coordinator/src/build_lease_reclaim.rs`, every 30s)
reconciles the capacity ledger and logs
`build_lease: reclamation pass over occupying leases`.

If admission denies with a non-zero `occupancy` while the namespace holds no
matching Pods or Jobs, read that line: `occupying` is what the cap is compared
against, `ownerless_dispatch` is the `task_dispatch` share retired on a durable
ownership proof, and `blockers` means the pass could not be trusted at all
(usually an unusable Kubernetes LIST).

Leases are only retired on proof — a provably absent Kubernetes object, a
terminal admission generation, or an unclaimed grant — so a degraded API server
or an unreadable ledger leaves every slot occupied by design.

### 5. Absent row (no authority at all)

Deleting the singleton is the immediate remediation for a wedged authority, so a
deployment can legitimately be running with **no** row. That is safe and
fail-closed: every invocation runs `Unleased` — no quota of its own, inheriting
the Pod's budget, still contained and still killable — and nothing can be armed
until the row is restored.

- **Detect:** `epoch show` prints
  `invocation lease authority: <absent> (disarmed; run `seed` to create it)`.
- **Recover:** `djinn-server epoch seed`. It creates the row DISARMED
  (`mode = off`, cap unset), then `epoch arm --mode enforce --cap N`. The command
  is idempotent: an existing row is reported and left untouched, so it can never
  disturb a live rollout.
- **Roll back:** `DELETE FROM admission_handoff WHERE name = 'build';` returns the
  deployment to the disarmed behaviour above. This is the one supported raw-SQL
  escape hatch, and it exists precisely because startup must never re-create the
  row implicitly — no migration or startup path does.

### 6. `epoch show` is armed but the pods still log `decision=Unleased`

The control plane and the pod can disagree, and `epoch show` cannot detect it: it
reads the row, while the decision that governs `cpu.max` is made **inside each
task-run Pod**. Verify the pod, not the row.

- **Detect:** in the `worker` container's log, one line per invocation:

  ```text
  INFO djinn_agent::process: lease invocation launched into a cgroup leaf
    invocation_id=… task_run_id=… decision=Lift authority=Armed threshold_usec=250000
  ```

  With `mode = enforce`, `decision` MUST be `Lift`.
  `decision=Unleased authority=Unarmed` against such a row means the pod is not
  seeing the authority you are looking at.
- **Confirm on the cgroup, not the log:** in the `cgroup-launcher` container, a
  leased leaf under `/run/djinn-cgroup/` is born at `25000 100000` (250m) and then
  transitions when it escalates. A leaf born at `max 100000` that never changes is
  `Unleased`; `nr_throttled` staying 0 across many invocations while builds run at
  ~1 CPU is the same finding.
- **Recover:** look for the
  `ERROR … durable invocation-lease authority read FAILED` line in the same
  container. That is a defect, not a disarmed authority — the pod cannot read the
  row at all (wrong DSN, missing migration, connectivity). The pod's platform
  connection comes from `DJINN_DATABASE_URL`; a task-run Pod also carries a
  project `DATABASE_URL` pointing at its `svc-postgres` catalog-service sidecar,
  which has no such table. An
  `INFO … no durable invocation-lease authority row` line instead means the row
  really is absent — see §5.
- **History:** the decision used to be a defaulted `SupervisorServices` trait
  method, and the in-pod launcher path resolved it through `RpcServices`, which
  never overrode it. Every invocation therefore read `Unleased` from a fully armed
  authority and no quota was ever lifted, with no log to say so. It is now a
  mandatory injected authority (`InvocationLiftAuthority`) and both non-lifting
  reads are logged.

## Alarm: refused cgroup lifts

`djinn_build_admission_lift_rejected_total` counts authorized lifts the
privileged launcher broker **refused**. Any non-zero value means armed
invocations are running permanently clamped at the 250m unleased quota (a ~16x
slowdown): the authority authorized the escalation, the invocation held a matching
durable grant, and the kernel-boundary control still came back refused.

The runner **degrades** rather than failing the command — the child keeps running
clamped — so this counter and its accompanying `tracing::error!`
(`lease invocation lift REFUSED by the launcher broker`) are the only signal that
the escalation path is broken. `djinn-agent-worker` exposes no `/metrics`
endpoint, so in a task-run Pod the log line is what escapes; grep for it.

The error carries a `ControlRejection` category naming *why* the broker refused
(`Fence`, `Unarmed`, `AlreadyLifted`, `Terminal`, `Nonce`, …). Before goxi
blocker 14 every refusal was reported as `InvalidControl` — which is a real and
*different* broker error — and the misattribution cost the whole investigation.

If the category is `Fence`, the worker's `BEGIN` and `LIFT` controls disagree
about the invocation fence; see `djinn-agent/src/process_broker.rs`. If it is
`Unarmed`, the leaf was born under an `Unleased` decision and no lift was ever
possible for it, which points back at the authority, not the launcher.

## Reading the shadow window

In `shadow` the runtime records both arms of
`djinn_telemetry::build_admission::record_shadow_invocation`
(`djinn_build_admission_shadow_invocation_total`, label `decision`), from
`server/crates/djinn-agent/src/process.rs`:

- `would_escalate` — the spawn crossed the escalation threshold and reached a
  valid matching durable bind, so the authority *would* have lifted the quota.
- `would_throttle` — the spawn ran to terminal without ever crossing
  `cpu_usage_threshold_usec`, so it was never escalated to the lease authority
  and would have been left throttled.

The two arms are mutually exclusive (escalation requires a grant, which requires
queueing), so `would_escalate / (would_escalate + would_throttle)` over the
window is the fraction of observed invocations arming would escalate. Both are
observation only: shadow never lifts `cpu.max` and never denies.

### Remaining gap (deferred to `u2oz`)

`would_throttle` covers invocations that never escalated, not the narrower case
of a shadow request the authority would have *denied because the reference cap is
already met* — shadow still takes a real lease grant rather than a non-enforcing
would-decision, so cap-denial is not distinguished. Measuring that needs a
non-enforcing broker capacity check on the shadow request path and belongs with
the cross-component work in `u2oz`.
