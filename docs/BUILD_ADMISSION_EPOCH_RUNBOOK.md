# Build-admission epoch operator runbook

Operator diagnostics and recovery for the durable **admission epoch** that owns
the Build Leases v1 rollout (epic `23or`, proposal `goxi`). The epoch is a single
durable row (`admission_handoff`, singleton `name = 'build'`) carrying the v0/v1
authority modes, the reference cap, the phase, and the per-authority + per-generation
acknowledgements. One serialized compare-and-swap owns all of them, so a cap
change and a phase advance can never interleave into a contradictory committed
state.

This runbook is diagnosis-and-recovery only. It never asks an operator to hand-edit
the durable row with raw SQL; the safe-ordering executor and the `epoch` admin
commands are the supported surface.

## Authorities, modes, and phases

- **v0 (emergency)** — the coordinator `BuildAdmissionController`. Modes:
  `enforce` / `observe` / `disabled`. Only `enforce` denies over the cap.
- **v1 (invocation)** — the per-spawn launcher quota lift, read directly from the
  epoch by the agents via `evaluate_invocation_lift`. Modes: `off` / `shadow` /
  `enforce`. Only `enforce` lifts the reserved `cpu.max`.
- **Phases** — `emergency_primary` → `forward_overlap` → `invocation_primary` →
  `rollback_overlap` → `emergency_primary`. Baseline is `emergency_primary` with
  `v0 = enforce, v1 = off`; `shadow` is that same baseline with `v1 = shadow`.

**Invariant:** every committed state has at least one enforcing authority. The
illegal combination in which neither authority enforces
(`v0 ∈ {observe, disabled} ∧ v1 ∈ {off, shadow}`) is rejected up front by
`validate_admission_config` and, if it is ever observed durably, fails closed via
`evaluate_handoff` (`HandoffState::IllegalModeCombo`).

## Operator commands

Run against a coordinator image with a current schema. These are one-shot admin
modes of the server binary; they open the DB, act, print, and exit before any
actor or HTTP listener starts.

```
djinn-server epoch show                  # print the durable row + derived state
djinn-server epoch seed                  # create the row when it is ABSENT (idempotent)
djinn-server epoch advance --cap N       # perform the next safe FORWARD step
djinn-server epoch set-cap --cap N        # change the reference cap (epoch-fenced)
djinn-server epoch rollback --cap N        # perform the next safe ROLLBACK step
djinn-server epoch kill-switch --cap N      # rollback from invocation-primary, urgent
```

`advance`, `rollback`, and `kill-switch` each perform exactly **one** safe step
and report what they did and what they are waiting on (a v0 controller-replica
ack, or the live-generation acks). Re-run the command after the awaited
acknowledgement lands; the epoch fence makes re-runs safe. Under the hood these
compose `AdmissionTransitionExecutor` in
`server/crates/djinn-coordinator/src/build_admission_transition.rs`.

### Forward cutover order

1. `arm_shadow` — `v0 = enforce, v1 = shadow`, phase stays `emergency_primary`.
2. `arm_overlap` — `v0 = enforce, v1 = enforce`, phase stays `emergency_primary`.
3. `enter_forward_overlap` — advance to `forward_overlap` **after** the v0
   controller-replica acks the epoch.
4. `commit_invocation_primary` — advance to `invocation_primary` **after** modes,
   cap, and **every** armed live generation have acknowledged the current epoch.
5. `observe_v0` — `v0 = observe, v1 = enforce`. Terminal forward state.

### Reverse kill-switch / rollback order

The reverse ordering is the safety-critical part. It **confirms v0 is enforcing
with a controller-replica ack before any transaction lifts or disables v1
quota**, and if v0 cannot be confirmed it **halts with both authorities
enforcing and never disables v1**.

1. `arm_rollback` — commit `v0 = enforce, v1 = enforce` at a **same-or-lower**
   cap (a rollback must not raise the cap). v1 keeps enforcing; nothing disabled.
2. `enter_rollback_overlap` — advance `invocation_primary` → `rollback_overlap`
   **only** once v0 is confirmed enforcing (`v0 = enforce` and
   `emergency_ack_epoch == epoch`). If not, the step returns
   `HaltedV0Unconfirmed`, makes no mutation, and leaves both enforcing.
3. `complete_rollback` — confirm v0 again, advance `rollback_overlap` →
   `emergency_primary` (v1 stops lifting the instant the phase is
   `emergency_primary`), then set `v1 = off`.

## Diagnosis and recovery procedures

### 1. Fail-closed suspect rows and forced exact-UID deletion

A build lease can land in `suspect` when the coordinator cannot confirm a lease
holder's terminal state (`BuildLeaseState::Suspect`, one of the five occupying
states in `server/crates/djinn-db/src/repositories/build_lease.rs`). A suspect row
keeps counting against the cap on purpose: it is fail-closed until the exact pod
is proven gone.

- **Detect:** `epoch show` reports the effective cap; compare against occupancy.
  Suspect rows appear in `build_lease` with `state = 'suspect'` and in the
  `djinn_build_lease_*` occupancy telemetry.
- **Recover:** do not delete the durable row. Force termination of the **exact**
  pod UID through the watchdog termination seam
  (`WatchdogTerminationRequest` handled in
  `server/crates/djinn-agent/src/direct_services.rs` and
  `server/crates/djinn-agent-worker/src/worker_services.rs`). This deletes only
  the pod whose UID matches — it refuses on empty/mismatched task, task-run, or
  pod UID — so a stale row can never reap a live successor. The reclamation of
  the exact predecessor UID on restart runs through
  `AdmissionJournalRepository` with `UidFencedAdmissionInput`
  (`server/crates/djinn-db/src/repositories/admission_journal.rs`); the row stays
  counted until that fenced terminal write lands.

### 2. Graph-warm candidate cleanup

A warm consumer that grabbed a launching grant but never bound an immutable pod
UID leaves a candidate that must be cleaned before its slot frees.

- **Detect:** the warm adapter
  (`server/crates/djinn-coordinator/src/graph_warm_lease.rs`) surfaces
  `candidate_cleanup` on the durable row; the Kubernetes side is inventoried by
  `server/crates/djinn-k8s/src/warm_job.rs`.
- **Recover:** let the reconciler
  (`BuildAdmissionReconciler::reconcile` in
  `server/crates/djinn-coordinator/src/build_admission_inventory.rs`) adopt live
  and release terminal warm workloads. Unclassifiable non-terminal build
  workloads become `blockers` in the `InventoryReport` and hold the gate closed
  fail-closed until relabeled or removed.

### 3. Cap zero (drain without disabling admission)

- **Do NOT** set the cap to zero through `epoch set-cap` — the epoch cap is
  validated to `[MIN_ADMISSION_CAP, MAX_ADMISSION_CAP]` (`1..=4096`) and the
  durable CHECK constraint rejects `cap <= 0`. A zero epoch cap is not a legal
  epoch state.
- To drain, use the lease service's `BuildLeaseService::set_cap(0)`
  (`server/crates/djinn-coordinator/src/build_lease.rs`), which stops granting new
  leases while occupied rows drain naturally. Light (coordinator-free) commands
  stay unaffected. Restore the cap with the same call once drained.

### 3b. `set-cap` takes effect without a restart

`epoch set-cap` changes what the RUNNING coordinator enforces. The durable
reference cap is re-read and adopted by
`BuildLeaseService::refresh_epoch_cap()` on the build-lease maintenance tick
(30s) and again on the handoff tick (5 min), and a raised cap immediately drains
the FIFO so work refused at the old cap is granted rather than waiting for the
next dispatch attempt. Lowering the cap stops granting but never revokes an
occupying row.

This was not always true. Before this fix the resolved cap was only written to
the enforcing atomic by `recover()`, so a `set-cap` reported success, `epoch
show` read back the new value, and every denial kept quoting the old one until
the pods restarted (production, 2026-07-25: `set-cap --cap 12`, denials still
`occupancy=3 cap=3`). If you see that shape again, the denial's `cap` field —
not `epoch show` — is the number actually in force.

### 3c. Occupancy that no workload is using

A build slot is a `build_leases` row, NOT an `admission_journal` row. The two
ledgers have separate reconcilers and can disagree:

- `BuildAdmissionReconciler` (startup + `initialize_build_admission_inventory`)
  reconciles the v0 lifecycle journal and logs
  `build_admission: reconciliation released stale durable occupancy`.
- `BuildLeaseReclaimer`
  (`server/crates/djinn-coordinator/src/build_lease_reclaim.rs`, every 30s)
  reconciles the v1 capacity ledger and logs
  `build_lease: reclamation pass over occupying leases`.

**Only the second one frees a cap.** If admission denies with a non-zero
`occupancy` while the namespace holds no matching Pods or Jobs, read the
`build_lease` line: `occupying` is what the cap is compared against,
`ownerless_dispatch` is the `task_dispatch` share retired on a durable
ownership proof, and `blockers` means the pass could not be trusted at all
(usually an unusable Kubernetes LIST). A journal line reporting
`reclaimed=N` says nothing about whether capacity was freed.

Leases are only retired on proof — a provably absent Kubernetes object, a
terminal admission generation, or an unclaimed grant — so a degraded API server
or an unreadable ledger leaves every slot occupied by design.

### 4. Stale epochs

A stale epoch is any durable row whose current-phase acknowledgements are not at
the current epoch, or that cannot be read.

- **Detect:** the `djinn_build_admission_handoff_warning{reason}` gauge and the
  logged warning carry one of the three bounded reasons from
  `HandoffWarningReason`: `stale_epoch`, `unexpected_overlap`, `epoch_unreadable`.
  `epoch show` prints the phase, epoch, both ack epochs, and the derived
  `HandoffState`.
- **Recover:** a stale/incomplete/unreadable epoch is already fail-closed —
  `evaluate_handoff` returns `RequiredFailClosed` and `evaluate_invocation_lift`
  returns `Unleased`, so v0 keeps enforcing and v1 never lifts. Re-run the
  executor step for the current phase once the coordinator leader re-acknowledges
  the current epoch (the leader's live handoff loop,
  `finalize_build_admission_handoff`, writes the emergency ack when the controller
  is a healthy `enforce`). Never advance from a stale epoch: the compare-and-swap
  returns `InvalidTransition`.

### 5. Unsupported node / runtime readiness

Enforcement must never be enabled without kernel isolation and inventory proofs.

- **Detect:** `BuildAdmissionReadiness` (`server/crates/djinn-coordinator/src/build_admission.rs`)
  reports the gate that is not yet healthy — `JournalRecoveryIncomplete`,
  `JournalUnhealthy`, `InventoryPending`, `TopologyPending`,
  `SeededOccupancyAboveCap`, `CreateUnknownHealth`, or `ShutdownDraining`. Only
  `Healthy` allows a v0 ack. On the launcher side an unsupported node surfaces as
  `LeaseContainmentFailed` (the launcher refuses to lift without cgroup-v2
  containment).
- **Recover:** do not force an epoch advance while any gate is unhealthy — the v0
  ack is gated on `readiness.is_healthy()`, so `enter_forward_overlap` will keep
  returning `AwaitingEmergencyAck`. Resolve the underlying gate (recover the
  journal, complete the Kubernetes inventory, confirm single-active topology),
  then retry. A `LeaseContainmentFailed` node must be drained or fixed before it
  can host v1-enforcing spawns; until then the epoch keeps v1 unleased on that
  node fail-closed.

### 6. Absent row (no epoch at all)

Deleting the singleton is the immediate remediation for a wedged handoff, so a
deployment can legitimately be running with **no** row. This is safe but is one
step below baseline: `evaluate_handoff` maps `Ok(None)` to
`HandoffState::MissingRow` / `EmergencyAuthorityDecision::ConfiguredStandalone`,
which retains the configured standalone v0 mode and never denies — and the whole
v1 rollout is dormant, so shadow cannot be armed and no would-throttle ratio
accumulates.

- **Detect:** `epoch show` prints `admission handoff row: <absent>`.
- **Recover:** `djinn-server epoch seed`. It creates the `emergency_primary`
  baseline (`v0 = enforce, v1 = off`, cap unset) **born acknowledged for its own
  epoch**, so the deployment lands on the complete v0 baseline instead of the
  fail-closed incomplete-epoch state, and `epoch advance` is immediately usable.
  The command is idempotent: an existing row is reported and left untouched, so
  it can never disturb a live rollout.
- **Roll back:** `DELETE FROM admission_handoff WHERE name = 'build';` and
  restart the coordinator returns the deployment to the dormant standalone
  behaviour above. This is the one supported raw-SQL escape hatch, and it exists
  precisely because startup must never re-create the row implicitly — no
  migration or startup path does.
- **Expect on the next start:** a seeded row requires v0, so startup promotes
  even a configured `observe`/`off` controller to a fail-closed `enforce`
  (`require_enforcement`) and admission stays closed through
  `JournalRecoveryIncomplete` → `InventoryPending` → `TopologyPending` until the
  coordinator wins the advisory lock. `confirm_build_admission_topology` then
  opens the topology gate and writes the emergency ack in the same seam; the
  periodic handoff loop re-drives it every 5 minutes. Denials logged as
  `occupancy 0 reached cap N` during that window are the ordinary fail-closed
  startup state, not a full ledger.

### 7. `epoch show` is armed but the pods still log `decision=Unleased`

The control plane and the pod can disagree, and `epoch show` cannot detect it: it
reads the row, while the decision that governs `cpu.max` is made **inside each
task-run Pod**. Verify the pod, not the row.

- **Detect:** in the `worker` container's log, one line per invocation:

  ```text
  INFO djinn_agent::process: lease invocation launched into a cgroup leaf
    invocation_id=… task_run_id=… decision=Lift authority=Armed threshold_usec=250000
  ```

  With a `forward_overlap`/`invocation_primary`/`rollback_overlap` row at
  `v1 = enforce` whose required acks are all at the current epoch, `decision` MUST
  be `Lift`. `decision=Unleased authority=Unarmed` against such a row means the
  pod is not seeing the epoch you are looking at.
- **Confirm on the cgroup, not the log:** in the `cgroup-launcher` container, a
  leased leaf under `/run/djinn-cgroup/` is born at `25000 100000` (250m) and then
  transitions when it escalates. A leaf born at `max 100000` that never changes is
  `Unleased`; `nr_throttled` staying 0 across many invocations while builds run at
  ~1 CPU is the same finding.
- **Recover:** look for the `ERROR … durable admission_handoff read FAILED` line
  in the same container. That is a defect, not an unarmed epoch — the pod cannot
  read `admission_handoff` at all (wrong DSN, missing migration, connectivity).
  The pod's platform connection comes from `DJINN_DATABASE_URL`; a task-run Pod
  also carries a project `DATABASE_URL` pointing at its `svc-postgres`
  catalog-service sidecar, which has no `admission_handoff` table. An
  `INFO … no durable admission_handoff row` line instead means the row really is
  absent — see §6.
- **History:** the decision used to be a defaulted `SupervisorServices` trait
  method, and the in-pod launcher path resolved it through `RpcServices`, which
  never overrode it. Every invocation therefore read `Unleased` from a fully armed
  epoch and no quota was ever lifted, with no log to say so. It is now a mandatory
  injected authority (`InvocationLiftAuthority`) and both non-lifting reads are
  logged.

## Aborting a forward overlap

The phase machine is a strict forward cycle, so you cannot advance
`forward_overlap` backward. If you must abort a cutover **before**
`invocation_primary`, use `epoch set-cap`/mode change to set `v1 = off` while
`v0` stays `enforce`: v0 remains the sole enforcing authority (safe, fail-closed)
and no v1 quota is lifted. Re-arm shadow/overlap later to resume. A full reverse
rollback (`rollback_overlap`) is only defined from `invocation_primary`.

## Reading the shadow window

In `shadow` the runtime records both arms of
`djinn_telemetry::build_admission::record_shadow_invocation`
(`djinn_build_admission_shadow_invocation_total`, label `decision`), from
`server/crates/djinn-agent/src/process.rs`:

- `would_escalate` — the spawn crossed the escalation threshold and reached a
  valid matching durable bind, so v1 *would* have lifted the quota. It stays
  throttled under v0.
- `would_throttle` — the spawn ran to terminal without ever crossing
  `cpu_usage_threshold_usec`, so it was never escalated to the lease authority
  and v1 would have left it throttled.

The two arms are mutually exclusive (escalation requires a grant, which requires
queueing), so
`would_escalate / (would_escalate + would_throttle)` over the window is the
fraction of observed invocations a cutover would escalate. Both are observation
only: shadow never lifts `cpu.max` and never denies.

### Remaining gap (deferred to `u2oz`)

`would_throttle` covers invocations that never escalated, not the narrower case
of a shadow request the invocation authority would have *denied because the
reference cap is already met* — shadow still takes a real lease grant rather
than a non-enforcing would-decision, so cap-denial is not distinguished.
Measuring that needs a non-enforcing broker capacity check on the shadow request
path and belongs with the cross-component work in `u2oz`. It does not weaken v0
and does not affect the executor's shadow arm, which correctly sets
`v1 = shadow`.
