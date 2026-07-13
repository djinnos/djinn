# Shared-cache cleanup rollout confirmation checklist

This is the mechanically checkable operator confirmation artifact for proposal
`0nt2` closeout (epic `mwex`). It turns the canonical
[Shared-cache cleanup rollout and rollback runbook](SHARED_CACHE_CLEANUP_ROLLOUT.md)
into one signed record per component, so a reviewer can confirm each destructive
stage had a bounded dry-run evidence gate, an explicit enable/stop decision, and
a rollback action before the next stage began.

**This document is a record template, not proof of production execution.** A
checked box records that an operator signed a field; it does not assert that a
production action happened merely because the row exists. Repository-automated
proof (the [validation guard](#repository-automated-proof) below) only verifies
that this checklist and the runbook stay structurally consistent with landed
code — it cannot observe a production cluster.

## How to use this checklist

- Execute the runbook stages in the documented
  [required rollout order](SHARED_CACHE_CLEANUP_ROLLOUT.md#required-rollout-order).
  Do not collapse gates. Each row below corresponds to one stage and must be
  closed in order.
- Every row has the same fields: **owner**, **mode**, **expected bounded
  outcome/candidate/byte evidence**, **enable decision**, **stop decision**,
  **rollback action**, and an **operator signature/record**.
- Fields marked *operator record* are filled by the named human owner from
  observed cluster state. Fields marked *repository-automated* are verified by
  the guard script and must not be hand-edited to imply production proof.
- Production-cluster execution (kubectl, Zot, Prometheus, the shared PVC) is
  explicitly operator-owned. No acceptance criterion for this artifact requires
  live infrastructure access.

## Component rows

The six stable component names below are the cross-path observability matrix
identifiers. `zot_retention` is the externally executed registry action whose
bounded state is the deterministic preflight report; the other five are
coordinator-owned cleanup paths. `warm_fingerprint` is intentionally last and
gated — see its row.

### 1. `zot_retention`

| Field | Value |
| --- | --- |
| Owner | release operator |
| Stage | [Stage 0 — Zot dry-run and selected-image preflight](SHARED_CACHE_CLEANUP_ROLLOUT.md#stage-0--zot-dry-run-and-selected-image-preflight) |
| Mode | `dry_run` → `destructive` (only after gate) |
| Expected bounded evidence | Preflight report `mode`/`outcome` header is one of `disabled`/`disabled`, `dry_run`/`advisory`, `destructive`/`destructive_safe`, `destructive`/`destructive_blocked`, or `(any)`/`fetch_error`. Candidate/retained/deleted tag counts, projected reclaimed bytes, projected retained bytes, and selected-image safe/unsafe counts come from the deterministic report — **not** from a coordinator Prometheus metric (Zot retention is not in `djinn_cache_cleanup_*`). Selected images must remain pullable by retained tag or digest. |
| Enable decision | Set `imagePipeline.zot.retention.dryRun=false` only after dry-run preflight shows every selected image pullable, startup succeeds, catalog scope is only `djinn-image-*`, and the operator has observed one scheduled Zot GC interval. |
| Stop decision | Any `fetch_error` outcome, `destructive_blocked` preflight, an unsafe/unavailable selected image, or catalog scope drift. Stop before flipping `.dryRun=false`. |
| Rollback action | Set `imagePipeline.zot.retention.dryRun=true` (or `enabled=false`); redeploy; restore a needed image by rebuilding/publishing and reselecting through the normal image control path, then rerun preflight. Registry GC cannot be undone. |
| Operator signature / record | _operator record_ — operator name, approval, time window, preflight report attachment, selected-image pullability evidence, Helm values or `kubectl set env` command rendered. |

- [ ] Dry-run preflight report observed and attached (operator record).
- [ ] Selected-image pullability confirmed (operator record).
- [ ] Destructive enable decision recorded or explicitly deferred (operator record).

### 2. `sccache` (one-time `/cache/sccache` deletion)

This row covers the **operator-owned one-time deletion** of `/cache/sccache`,
which is distinct from the recurring coordinator guard. The recurring guard is
tracked in the `cargo_target_runs_debris`-adjacent stage; this row is the
irreversible manual deletion only.

| Field | Value |
| --- | --- |
| Owner | cache operator |
| Stage | [Stage 1 — prove build pods do not rely on sccache](SHARED_CACHE_CLEANUP_ROLLOUT.md#stage-1--prove-build-pods-do-not-rely-on-sccache) and [Stage 2 — operator-owned one-time `/cache/sccache` deletion](SHARED_CACHE_CLEANUP_ROLLOUT.md#stage-2--operator-owned-one-time-cachesccache-deletion) |
| Mode | one-time manual deletion (no `dry_run`/`delete` toggle; this is not the recurring guard) |
| Expected bounded evidence | Pre-delete: one task-run pod and one warm Job pod both show `CARGO_INCREMENTAL=1` and `RUSTC_WRAPPER=""` (empty). `SCCACHE_DIR` may exist as the namespaced fallback and is not proof sccache is in use. Deletion: `du -sh /cache/sccache` size, the `find ... -printf` inventory file, and the exact `rm -rf -- /cache/sccache` command result. Post-delete rebuild observation. |
| Enable decision | Execute the deletion only after Stage 1 is clean, an approved maintenance window is open, no build pod that might explicitly invoke sccache is active, and the mounted path is exactly `/cache/sccache` on the intended shared PVC. |
| Stop decision | Nonempty `RUSTC_WRAPPER` on any live pod, mounted path is not exactly `/cache/sccache`, mount is not the intended PVC, or any build pod is still a possible writer. |
| Rollback action | Deletion cannot be reversed. Remove the maintenance pod and allow the next explicitly-invoked sccache user to rebuild its namespaced cache. Warm/task-run performance recovery is through normal warm-base rebuilds, not restoration of copied sccache files. |
| Operator signature / record | _operator record_ — see the explicit fields below. |

**Explicit operator record fields for the one-time `/cache/sccache` deletion:**

- [ ] Pre-delete confirmation — operator name: __________
- [ ] Pre-delete confirmation — approval reference: __________
- [ ] Pre-delete confirmation — approved maintenance window (UTC start/end): __________
- [ ] Pre-delete confirmation — Stage 1 pod-environment evidence attached (task pod `RUSTC_WRAPPER=""`, warm pod `RUSTC_WRAPPER=""`): __________
- [ ] Pre-delete inventory — `du -sh /cache/sccache` output: __________
- [ ] Pre-delete inventory — `find` listing file path/name: __________
- [ ] Deletion — exact command result (`rm -rf -- /cache/sccache` exit code/output): __________
- [ ] Completion — post-delete rebuild observation (next sccache user / warm rebuild): __________

> These eight fields are operator-owned evidence and are deliberately separated
> from repository-automated proof. The validation guard below checks that the
> runbook contains the matching command shape and Stage 1 gate; it does not and
> cannot verify that a production deletion occurred.

### 3. `cargo_target_runs_debris`

This row covers **both** the recurring sccache guard and the malformed run-root
debris sweep, because they share the global `DJINN_CACHE_CLEANUP_MODE` switch
and the [Stage 3](SHARED_CACHE_CLEANUP_ROLLOUT.md#stage-3--recurring-sccache-guard-and-run-root-debris-cleanup) observation/enable gate.

| Field | Value |
| --- | --- |
| Owner | cache operator (telemetry review); release operator (Helm/env change) |
| Stage | [Stage 3 — recurring sccache guard and run-root debris cleanup](SHARED_CACHE_CLEANUP_ROLLOUT.md#stage-3--recurring-sccache-guard-and-run-root-debris-cleanup) |
| Mode | `dry_run` → `delete` (global `DJINN_CACHE_CLEANUP_MODE`) |
| Expected bounded evidence | Metrics component labels are exactly `sccache` and `cargo_target_runs` (the coordinator telemetry stable names). Candidates: `djinn_cache_cleanup_candidates_total{component,mode}`. Outcomes: `djinn_cache_cleanup_total{component,outcome,mode}` with outcomes including `deleted`, `skipped`, `retained`, `error`, `dry_run`, `uuid_orphan_deleted`, `malformed_dir_deleted`, `loose_file_deleted`, `retained_fresh_malformed`, `retained_non_utf8`. Bytes: `djinn_cache_cleanup_reclaimed_bytes_total{component,mode}`. Structured logs include `path`, `size_bytes`, `age_secs`, `threshold_secs`, `mode`, `cleanup_outcome`. No `error` outcomes, no unexpected fresh-writer warning. |
| Enable decision | Switch `DJINN_CACHE_CLEANUP_MODE=delete` only after dry-run metrics show expected `sccache` and `cargo_target_runs` candidates, logs show only intended stale paths, and no `error` outcomes. Observe one maintenance interval. |
| Stop decision | Any `error` outcome, unexpected fresh-writer warning, candidate/path/byte evidence differs from intended scope, or an unexpected non-stale path selected. |
| Rollback action | Immediately set `DJINN_CACHE_CLEANUP_MODE=dry_run`; to stop an individual recurring path also set `DJINN_CACHE_CLEANUP_SCCACHE_ENABLED=false` or `DJINN_CACHE_CLEANUP_CARGO_DEBRIS_ENABLED=false`. Deleted cache/run debris is rebuilt by normal warm or task execution; never recreate a deleted UUID run directory manually. |
| Operator signature / record | _operator record_ — operator name, approval, rendered Deployment env, time window, the three PromQL query outputs, structured-log excerpts, enable/rollback decision. |

- [ ] Dry-run sccache guard candidates/outcomes/bytes observed (operator record).
- [ ] Dry-run run-root debris candidates/outcomes/bytes observed (operator record).
- [ ] Delete-mode enable decision recorded or explicitly deferred (operator record).

### 4. `warm_idle`

| Field | Value |
| --- | --- |
| Owner | cache operator (telemetry review); release operator (env change) |
| Stage | [Stage 4 — warm-base idle eviction, then pressure eviction](SHARED_CACHE_CLEANUP_ROLLOUT.md#stage-4--warm-base-idle-eviction-then-pressure-eviction) (idle dry-run and idle enablement gate) |
| Mode | `dry_run` → `delete` (global `DJINN_CACHE_CLEANUP_MODE`) |
| Expected bounded evidence | Metrics component label is exactly `cargo_warm_base`. Dry-run log: `warm-base idle GC would delete idle base` with `project_id`, `size_bytes`, `mode`. Completion fields: `component`, `mode`, `deleted`, `dry_run`, `retained`, `reclaimed_bytes`, `projected_bytes`. Retention reasons: `retained_young`, `retained_active`, `retained_lock_busy`. Candidates/bytes via `djinn_cache_cleanup_candidates_total` and `djinn_cache_cleanup_reclaimed_bytes_total` for `component="cargo_warm_base"`. `projected_bytes` must fit the approved blast radius; no `error` outcomes; no seed-health regression. |
| Enable decision | Switch global mode to `delete` for one observed interval only after candidates are expected idle bases, retained reasons show safety guards working, `projected_bytes` fits the approved blast radius, and seed health is stable. |
| Stop decision | Unexpected retained reason, `projected_bytes` exceeds approved blast radius, any `error` outcome, or seed-health regression. |
| Rollback action | Set global mode back to `dry_run` (or restore previous conservative retention/watermarks), wait for rollout, record the last completion line and metric deltas. Whole-base deletion is irreversible; rebuild via the normal warm job for that project. Do not copy a warm base from another project. |
| Operator signature / record | _operator record_ — operator name, approval, env, time window, idle completion fields, metric deltas, seed-health review. |

- [ ] Idle dry-run candidates/projected bytes/retained reasons observed (operator record).
- [ ] Idle delete enable decision recorded or explicitly deferred (operator record).

### 5. `warm_pressure`

| Field | Value |
| --- | --- |
| Owner | cache operator (telemetry review); release operator (env change) |
| Stage | [Stage 4 — warm-base idle eviction, then pressure eviction](SHARED_CACHE_CLEANUP_ROLLOUT.md#stage-4--warm-base-idle-eviction-then-pressure-eviction) (pressure dry-run and pressure enablement gate) |
| Mode | `dry_run` → `delete` (global `DJINN_CACHE_CLEANUP_MODE`); pressure only starts below `WARM_BASE_LOW_FREE_RATIO` and stops after `WARM_BASE_HIGH_FREE_RATIO` |
| Expected bounded evidence | Metrics component label is exactly `cargo_warm_base`. Completion log: `warm-base pressure GC completed` with `component`, `mode`, `deleted`, `dry_run`, `retained`, `retained_outcomes`, `reclaimed_bytes`, `projected_bytes`, `reached_high_watermark`, `remeasurement_failed`. Dry-run must select an expected ordered prefix and report acceptable projected bytes. `remeasurement_failed` must be false. |
| Enable decision | Keep `delete` for a single observed pressure interval only after dry-run selects an expected ordered prefix, projected bytes are acceptable, low/high watermarks reviewed, and `remeasurement_failed` is false. |
| Stop decision | `remeasurement_failed` is true, errors rise, or the high-watermark behavior differs from evidence. |
| Rollback action | Set global mode back to `dry_run` (or restore previous conservative retention/watermarks), wait for rollout, record the last completion line and metric deltas. Whole-base deletion is irreversible; rebuild via the normal warm job for that project. Do not copy a warm base from another project. |
| Operator signature / record | _operator record_ — operator name, approval, env, time window, pressure completion fields, watermark review, metric deltas. |

- [ ] Pressure dry-run ordered prefix/projected bytes/watermarks observed (operator record).
- [ ] Pressure delete enable decision recorded or explicitly deferred (operator record).

### 6. `warm_fingerprint` (gated, last)

> **Fail-safe and last.** This row is intentionally gated on the `w06b` evidence
> gate and cannot be read as proof that destructive fingerprint cleanup already
> exists or is enabled. No rollout command, config knob, or deletion procedure
> for fingerprints is defined by this artifact or by the runbook. The only
> current fingerprint behavior relevant here is that private-target seeding
> copies fingerprint metadata while skipping incremental content. Checking the
> box below records only that fingerprint deletion was confirmed **disabled** (or
> dry-run-only if `w06b` supplies an explicitly reviewed dry-run implementation)
> and remains last — it is **not** an enablement record.

| Field | Value |
| --- | --- |
| Owner | `w06b` safety gate owner |
| Stage | [Fingerprint-last hold](SHARED_CACHE_CLEANUP_ROLLOUT.md#fingerprint-last-hold) (last, after Stages 0–4) |
| Mode | disabled (or dry-run-only if `w06b` supplies one) — **not** `delete` |
| Expected bounded evidence | No `warm_fingerprint` component exists in `djinn_cache_cleanup_*` metrics and no fingerprint deletion command/knob is documented in the runbook. The `w06b` evidence gate (empirical Cargo-mtime safety spike) has not landed; until it does, this row records a hold, not an enablement. |
| Enable decision | **None.** Do not enable fingerprint deletion. This checkbox cannot imply enablement before the `w06b` safety result lands. |
| Stop decision | Any attempt to add a fingerprint sweep, config knob, or deletion command to the runbook or coordinator is a stop condition. |
| Rollback action | N/A — nothing is enabled to roll back. If a future `w06b` implementation lands a reviewed dry-run, re-open this row against that implementation's evidence; do not retroactively mark it enabled here. |
| Operator signature / record | _operator record_ — operator name, confirmation that no fingerprint deletion is enabled, reference to the open `w06b` gate. |

- [ ] Fingerprint deletion confirmed disabled/dry-run-only pending `w06b`; no implementation claimed or duplicated here (operator record).

## Repository-automated proof

The following is verified by `scripts/check-shared-cache-rollout.sh` (self-tested
by `scripts/test-check-shared-cache-rollout.sh`). It runs without Kubernetes,
Zot, Prometheus, or production filesystem access — it only reads the checked-in
runbook and this checklist and asserts structural consistency with landed
component/mode/outcome names and embedded config examples.

- The runbook `docs/SHARED_CACHE_CLEANUP_ROLLOUT.md` exists and contains every
  required stage heading.
- This checklist contains exactly the six stable component names
  (`zot_retention`, `sccache`, `cargo_target_runs_debris`, `warm_idle`,
  `warm_pressure`, `warm_fingerprint`) as row headings, in rollout order, with
  `warm_fingerprint` last and gated.
- The cross-links between this checklist and the runbook resolve to real
  anchors.
- The embedded Helm/PromQL/kubectl config examples in the runbook reference the
  exact env-var and metric names landed in the coordinator and telemetry crates.
- The fingerprint row cannot be read as enablement before `w06b`.

Run the guard from the repository root:

```sh
sh scripts/check-shared-cache-rollout.sh
sh scripts/test-check-shared-cache-rollout.sh
```

## Related

- Canonical runbook: [SHARED_CACHE_CLEANUP_ROLLOUT.md](SHARED_CACHE_CLEANUP_ROLLOUT.md)
- Per-task-run directory lifecycle: [CARGO_TARGET_RUN_DIR_VALIDATION.md](CARGO_TARGET_RUN_DIR_VALIDATION.md)
- Zot retention/GC observation guidance: `server/docs/operational/zot-retention-gc-observation.md`
- Proposal `0nt2`: Bound VPS shared-cache growth.
- Epic `mwex`: Shared-cache cleanup rollout runbook and cross-path observability closeout.
- Epic `w06b`: Warm Cargo target fingerprint staleness sweep (open; gates `warm_fingerprint`).
