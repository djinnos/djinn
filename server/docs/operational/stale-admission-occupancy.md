# Stale Build-Admission Occupancy

> **Two different failures land here.** A build-admission denial is either a
> **readiness** denial (`occupancy="unmeasured"`) or a **capacity** denial
> (`occupancy=<number>`, `cause="at_capacity"`). They share no remedy. Start
> with "which failure is this?" below; the stale-occupancy procedures further
> down apply only to the second.

The durable admission journal (`admission_journal`) is the grant authority for
build-producing workloads once `buildAdmission.mode` is armed to `enforce`. Rows
in `reserved`, `create_in_flight`, `create_unknown` or `live` occupy capacity
against the shared task/warm cap. Terminal rows are retained as history and
occupy nothing.

A row leaves an occupying state in exactly three ways:

1. a lifecycle callback from the process that created the object,
2. startup recovery, which retires predecessor `reserved` rows and converts
   predecessor `create_in_flight` into occupying `create_unknown`,
3. startup reconciliation, which retires a row whose Kubernetes object the API
   server proves does not exist.

Before (3) existed, a row whose object died together with its creating process
occupied capacity permanently. That is how a fleet accumulates a stale
population. On 2026-07-25 production held **318** occupying rows while
`kubectl get jobs -A` returned no Djinn Jobs at all.

## First: which failure is this? `unmeasured` is not `at_capacity`

These are completely different failures and the coordinator log distinguishes
them on one field. Read that field before anything else.

```
build admission denied; leaving task queued  occupancy="unmeasured" cap=3
                                             cause="controller_not_admitting"
```

`occupancy="unmeasured"` means the denial **never consulted capacity at all**.
The controller failed closed on a readiness gate and returned before any
occupancy was read. The pool may be completely idle. Nothing about the cap, the
lease ledger, or the number of running Jobs is relevant, and every capacity
remedy in this document is the wrong tool.

```
build admission denied; leaving task queued  occupancy=3 cap=3
                                             cause="at_capacity"
```

`occupancy=<number>` with `cause="at_capacity"` is the genuine capacity denial:
weighted occupancy plus this request's weight exceeded the cap. That is the
failure the rest of this runbook addresses.

The `occupancy` field is `Option` end to end precisely so these cannot be
confused. Before #2661 every non-capacity denial printed a hard-coded `0`, and
a permanently tombstoned lease spent forty minutes looking like a full pool at
`occupancy=0 cap=3`. **If you ever see a literal `0` occupancy alongside a
denial, distrust it before you distrust anything else.**

The same distinction on the operator surfaces:

| Surface | `unmeasured` (readiness) | `at_capacity` |
| --- | --- | --- |
| `/debug/dispatch-state` | `build_admission.occupancy: null`, `is_ready: false`, `totals.build_admission_denying_all: true` | `build_admission.is_ready: true`, `occupancy >= effective_cap` |
| `board_health` `dispatch_gate` | `build_admission_denial.cause = "controller_not_admitting"` with a `readiness`; `build_admission.create_unknown_settled > 0` | `build_capacity.at_capacity: true`, or `build_lease_queued` |
| doctor | `build_admission_health` finding | `build_admission_health` reports `readiness: healthy` |

`board_health`'s `build_capacity` block speaks **only** for the lease authority
(`admission_handoff.v1_mode`), which is why its `enforcing` field was renamed
`lease_authority_enforcing`. A healthy `build_capacity` is not evidence that a
dispatch can proceed: the readiness gate runs first and denies before those
numbers are ever reached.

## Reading the readiness, and whether it self-heals

`readiness` is the bounded reason `admit()` gates on, evaluated in fail-closed
priority order — the FIRST failing gate wins, so clearing it can simply reveal
the next one. `/debug/dispatch-state` reports `unsatisfied_gates` with the whole
set for exactly that reason.

Get it without node access:

```bash
curl -s -H "cookie: djinn_session=$ADMIN_SESSION" \
     http://<server>:3000/debug/dispatch-state | jq .build_admission
```

```
djinn MCP: doctor_run     # the build_admission_health check
```

| Readiness | Meaning | Self-heals? |
| --- | --- | --- |
| `journal_recovery_incomplete` | Startup default. Predecessor recovery has not finished. | **Yes**, in seconds, unless recovery is failing — then it becomes `journal_unhealthy`. |
| `journal_unhealthy` | A recovery or seed query against Postgres failed. | **Yes**, on the next successful reconciliation pass. If it persists, the database is the problem; fix that first. |
| `create_unknown_health` | ≥1 **recovered** `admission_journal` row is in `create_unknown`: a create was POSTed and its object UID never learned. | **Usually.** Reconciliation retires the row once it settles (300 s) and the API server proves the object absent. Since #2746 only predecessor-epoch rows arm this. **This is the 2026-07-29 gate.** |
| `seeded_occupancy_above_cap` | Occupied build slots exceed the cap in force. | **Yes**, as work completes or reconciliation reclaims. If not, this is the stale-occupancy failure the rest of this runbook covers. |
| `inventory_pending` | The broad Kubernetes workload LIST has not completed, or failed. | **Yes**, on the next successful pass. Persistent means the API server is unreachable or RBAC is wrong. |
| `topology_pending` | This process has not won the coordinator leadership race. | **Yes** on the leader. **Never on a standby** — a standby is *supposed* to sit here. Check you are querying the leader before treating it as a fault. |
| `shutdown_draining` | Graceful shutdown. New reservations are blocked by design. | N/A — the process is going away. |
| `healthy` | Every gate passes; admission proceeds to capacity. | — |

`seconds_since_last_reconcile` on the same payload is the other half of the
picture: a closed gate plus a large (or `null`) reconcile age means the thing
that would clear it is not running. `null` means **no pass has ever completed
in this process**, which is louder than a large number, not quieter.

`blocking_identities` names WHICH rows hold `create_unknown_health` closed, in
`{domain}:{work_id}:{generation}@{object_name}` form, so the row can be found in
Kubernetes without touching the database.

## Worked example: 2026-07-29, five hours of board-wide denial

The whole board stopped dispatching. Every coordinator tick logged
`occupancy:"unmeasured" cap:3 cause:"controller_not_admitting"`, and every
reconciliation pass logged `stale:0` — the row holding the gate closed was never
even a reclamation candidate.

| Time (UTC) | Event |
| --- | --- |
| 06:22:57 | A routine dispatch tick lands inside one task-run's POST→session window. `finish_task_run_build_admission` had written `create_unknown` for it (normal), the tail seed counted it, and `CreateUnknownHealth` armed against the live process's **own healthy work**. |
| 06:22:57 → 11:37 | The worker never registered a session, so `mark_live` never ran and the Job was TTL-reaped. `is_reclaimable` refuses to retire a row under this process's own epoch, so the gate had **no reachable clearing path**. 152 reconciliation passes ran; every one reported `stale:0`. |
| 11:37 | Server restarted. Recovery reclassified the row as a predecessor's — which *should* clear it — but the `create_in_flight → create_unknown` relabel stamped `updated_at = now()`, re-arming the 300 s settle window on the very row the restart had just made reclaimable. A pass at t+296.2 s missed by 3.8 s. |
| ~11:46 | The next pass cleared it. Board resumed. |

Diagnosis required ssh to the node and `grep readiness=` over container logs.
Every operator surface was silent or misleading: `/health` returned
`{"status":"ok"}` throughout, `/debug/dispatch-state` did not mention build
admission at all, `board_health` reported `gate_verdict: "unexplained"` with
`reasons: []` once every thirty seconds, and `build_capacity` reported
`{occupancy: 1, cap: 3, enforcing: true, at_capacity: false}` — all true, all
irrelevant.

The underlying defect is fixed in #2746 (arming and reclamation now agree on one
population; the relabel no longer stamps `updated_at`). The visibility gaps are
closed by #2747 and the follow-up that added the sections above.

### What does NOT work as a remedy

Reach for these and you will lose time:

* **`dispatch_resume`** clears the manual-pause row. That is a *different* gate.
  Against a readiness denial it is a no-op that looks like the remedy.
* **`board_reconcile`** touches sessions and tasks only. It does not run an
  admission reconciliation pass.
* **`doctor_fix`** returns `FixNotSupported` for this finding.
* **Raising the cap.** The denial never measured occupancy; the cap is not
  involved.

### What does work

1. Wait one reconciliation interval (120 s). Most readiness states clear on
   their own — check the table above before acting.
2. If `create_unknown_health` persists past two passes, read
   `blocking_identities` and confirm in the cluster that those objects are gone:
   ```bash
   kubectl get job -n djinn <object-name>
   ```
   Reconciliation will retire a row only once it has settled 300 s **and** the
   API server proves the object absent.
3. A rolling restart re-runs recovery and reconciliation and is the standard
   escalation — but note it also resets the settle clock, which is why the
   2026-07-29 board stayed down ten minutes past the restart. Expect a delay of
   at least the settle window before it helps.
4. Only then fall back to the operator cleanup below, and only under its
   preconditions.

## Symptoms

* `djinn_build_slots_in_use` is far above the configured cap while the namespace
  holds no `djinn-warm-*` or `djinn-taskrun-*` Jobs.
* `djinn_build_admission_occupancy_over_cap` is `1`.
* `djinn_build_admission_stale_rows` is above the cap after a reconciliation
  pass.
* Readiness reports `create_unknown_health` or `seeded_occupancy_above_cap`, and
  with `mode=enforce` every admission is denied. Since #2661 such a denial
  reports `occupancy="unmeasured"` rather than the self-contradictory
  `occupancy 0 reached cap 3` it used to print — see the section above.
* `board_health` names it: `dispatch_gate.reasons` contains
  `admission_create_unknown_pending` or
  `build_admission_denied_controller_not_admitting`, and
  `dispatch_gate.build_admission_denial.readiness` carries the closed gate.
* Server logs carry a single loud line rather than thousands of warnings:
  `build_admission: durable occupancy exceeds the configured cap`.

## Diagnosis

Read the bounded telemetry; do not query the database.

```promql
# Durable occupancy vs the armed cap.
djinn_build_slots_in_use
max(djinn_build_admission_occupancy_over_cap) by (effective_mode, effective_cap)

# Size of the stale population the last reconciliation pass proved absent.
djinn_build_admission_stale_rows

# Reconciliation is running and is not being fenced off.
sum by (outcome) (
  increase(djinn_build_admission_transition_total{outcome=~"reclaimed|reclaim_fenced"}[1h])
)
```

Cross-check against the cluster:

```bash
kubectl get jobs -n djinn
kubectl get pods -n djinn -l djinn.app/component=task-run-worker
```

If `djinn_build_slots_in_use` is large and the namespace holds no matching Jobs,
the occupancy is stale.

## Normal remediation: let reconciliation run

Startup reconciliation runs on the leader after journal recovery, on every
process start. It reclaims a row only when **all** of the following hold:

* the row's `creator_server_epoch` differs from the running process's epoch, so
  no in-process dispatch can still be mid-create for it;
* the row has been untouched for at least the settle window
  (`DEFAULT_RECLAIM_SETTLE_WINDOW`, 5 minutes), so the API server can no longer
  be admitting a create the dead process POSTed;
* the authoritative Job LIST contains no object under the row's `object_name`;
* a direct GET for that name answers `Absent`. A GET that fails (`Uncertain`)
  is never treated as proof, so a degraded API server reclaims nothing.

**Therefore the first remediation is always a rolling restart of the server**,
which runs a reconciliation pass:

```bash
kubectl rollout restart deploy/djinn-server -n djinn
kubectl rollout status deploy/djinn-server -n djinn --timeout=300s
```

Verify:

```bash
kubectl logs -n djinn deploy/djinn-server -c djinn-server \
  | grep 'Kubernetes inventory reconciliation complete'
```

The line reports `adopted`, `released`, `reclaimed`, `stale`, `fenced` and the
resulting `readiness`. Then confirm the gauge:

```bash
curl -s http://<server-pod-ip>:3000/metrics | grep djinn_build_slots_in_use
```

`djinn_build_slots_in_use` must equal the number of Djinn Jobs actually running.

## Fallback: one-time operator cleanup

Use this **only** when reconciliation cannot run — for example the cluster's API
server is unreachable so every probe returns `Uncertain`, or the population
predates the reconciliation path and the cluster no longer has any record of the
objects. Reconciliation is always preferred because it is evidence-fenced and
this procedure is not.

### Preconditions (all must hold)

1. `kubectl get jobs -n djinn` lists **no** `djinn-warm-*` and no
   `djinn-taskrun-*` Jobs. Record the output; it is the evidence for the write.
2. `kubectl get pods -n djinn` shows no task-run or warm Pods.
3. Dispatch is paused, so no new row can be created mid-procedure:
   ```
   djinn MCP: dispatch_pause
   ```
4. A database backup exists for the current point in time.

### Procedure

Take the backup snapshot of the rows first — this **is** the rollback artifact:

```sql
CREATE TABLE admission_journal_stale_backup_20260725 AS
SELECT * FROM admission_journal
WHERE state IN ('reserved', 'create_in_flight', 'create_unknown', 'live');
```

Confirm the count matches what the metric reported:

```sql
SELECT count(*) FROM admission_journal_stale_backup_20260725;
```

Then retire the population, scoping the write by the same evidence:

```sql
UPDATE admission_journal
SET state = 'terminal', terminal_at = now(), updated_at = now()
WHERE state IN ('reserved', 'create_in_flight', 'create_unknown', 'live')
  AND updated_at < now() - interval '1 hour';
```

The `updated_at` clause is a floor, not the justification: the justification is
precondition 1. It exists so a row created while the statement was being typed
cannot be caught.

Restart the server so the controller re-seeds from the corrected journal:

```bash
kubectl rollout restart deploy/djinn-server -n djinn
kubectl rollout status deploy/djinn-server -n djinn --timeout=300s
```

Resume dispatch:

```
djinn MCP: dispatch_resume
```

### Verification

1. `djinn_build_slots_in_use` reads `0` (or the true number of running Jobs).
2. `djinn_build_admission_occupancy_over_cap` reads `0`.
3. `djinn_build_admission_create_unknown_health` reads `0`.
4. The startup log line reports `readiness=Healthy`.
5. A task dispatches and reaches a running Pod.

### Rollback

If the board misbehaves after the cleanup, restore the exact prior states from
the backup table and restart:

```sql
UPDATE admission_journal AS j
SET state = b.state,
    terminal_at = b.terminal_at,
    updated_at = b.updated_at
FROM admission_journal_stale_backup_20260725 AS b
WHERE j.domain = b.domain
  AND j.work_id = b.work_id
  AND j.generation = b.generation;
```

```bash
kubectl rollout restart deploy/djinn-server -n djinn
```

Restoring re-occupies the capacity, which re-wedges Enforce. Roll the mode back
to `observe` in the same window if you need the board to keep running while the
underlying cause is investigated.

Drop the backup table only after a full day of healthy operation:

```sql
DROP TABLE admission_journal_stale_backup_20260725;
```

## Why not an age cutoff

An occupying row that looks old is not evidence that its work is finished. A
warm build is a heavy compile and can legitimately run for a long time, and a
`live` row for a running Job must keep occupying capacity for exactly as long as
that Job runs. Every automated release in this system is fenced on the API
server's answer about a specific object; the settle window only prevents racing
a create that is still being admitted. The operator procedure above is the sole
exception, and it substitutes a human-verified `kubectl get jobs` for the
automated probe.
