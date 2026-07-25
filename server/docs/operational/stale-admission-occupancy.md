# Stale Build-Admission Occupancy

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

## Symptoms

* `djinn_build_slots_in_use` is far above the configured cap while the namespace
  holds no `djinn-warm-*` or `djinn-taskrun-*` Jobs.
* `djinn_build_admission_occupancy_over_cap` is `1`.
* `djinn_build_admission_stale_rows` is above the cap after a reconciliation
  pass.
* Readiness reports `CreateUnknownHealth` or `SeededOccupancyAboveCap`, and with
  `mode=enforce` every admission is denied with the self-contradictory
  diagnostic `occupancy 0 reached cap 3` (the in-memory occupancy the denial
  message renders is not the durable occupancy that produced the denial).
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
