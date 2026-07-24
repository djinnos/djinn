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

## Aborting a forward overlap

The phase machine is a strict forward cycle, so you cannot advance
`forward_overlap` backward. If you must abort a cutover **before**
`invocation_primary`, use `epoch set-cap`/mode change to set `v1 = off` while
`v0` stays `enforce`: v0 remains the sole enforcing authority (safe, fail-closed)
and no v1 quota is lifted. Re-arm shadow/overlap later to resume. A full reverse
rollback (`rollback_overlap`) is only defined from `invocation_primary`.

## Shadow-mode observability gap (deferred to `u2oz`)

In `shadow` every user spawn traverses the broker, but the runtime currently
records only the `would_escalate` shadow-invocation decision
(`djinn_telemetry::build_admission::record_shadow_invocation`, called from
`server/crates/djinn-agent/src/process.rs`). The complementary `would_throttle`
decision (v1 *would* have kept the quota throttled because the reference cap is
already met) is **not** emitted: a shadow request that the invocation authority
would have denied is not distinguished, because shadow still takes a real lease
grant rather than a non-enforcing would-decision. This does not weaken v0 (shadow
never lifts and never denies) and does not affect the executor's shadow arm,
which correctly sets `v1 = shadow`. Wiring the `would_throttle` branch requires a
non-enforcing broker capacity check on the shadow request path and belongs with
the cross-component work in `u2oz`.
