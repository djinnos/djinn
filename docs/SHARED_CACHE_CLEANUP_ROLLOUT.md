# Shared-cache cleanup rollout and rollback runbook

This is the canonical operator runbook for shared-cache cleanup. It is a
**dry-run-first** procedure for the shared cache PVC. It describes repository
behavior, not a record of a production rollout. The focused
[shared-cache cleanup diagnosis runbook](SHARED_CACHE_CLEANUP_RUNBOOK.md)
defines the operative Helm/default, cap, pressure, and lock diagnoses. The companion
[`CARGO_TARGET_RUN_DIR_VALIDATION.md`](CARGO_TARGET_RUN_DIR_VALIDATION.md)
covers the per-task-run directory lifecycle and lock-contention check in more
detail.

> ## ⚠ Destructive Helm default — stop and read before installing or upgrading
>
> Shipped Helm **installs and upgrades default to
> `cacheCleanup.mode=delete`**. That value authorizes destructive coordinator
> cleanup; it is not a dry-run-first chart default. Before the first Helm
> install/upgrade in an observation rollout, explicitly override it to
> `dry_run`:
>
> ```bash
> export NS=djinn
> export RELEASE=djinn
> helm upgrade --install "$RELEASE" deploy/helm/djinn --namespace "$NS" \
>   --create-namespace --set cacheCleanup.mode=dry_run --wait
> ```
>
> To return a release to observation mode after an unsafe result, use the same
> explicit override (including `--reuse-values` for an existing release):
>
> ```bash
> helm upgrade "$RELEASE" deploy/helm/djinn --namespace "$NS" \
>   --reuse-values --set cacheCleanup.mode=dry_run --wait
> # Or restore the previously recorded Helm revision:
> helm rollback "$RELEASE" <previous-revision> --namespace "$NS" --wait
> ```
>
> A direct `djinn-server` binary is different: an unset or invalid
> `DJINN_CACHE_CLEANUP_MODE` fails safe to `dry_run`. Helm deliberately sets
> the shipped chart value, so do not infer the binary fallback from a rendered
> release.

## Required rollout order

Do not collapse these gates into one deployment. Execute and record them in
this order:

1. Deploy the conservative coordinator settings in `dry_run` and inspect the
   bounded cleanup telemetry.
2. Run Zot selected-image preflight in dry-run and observe Zot retention/GC.
3. Confirm current task-run and warm pods have `RUSTC_WRAPPER=""`.
4. Have the cache operator perform and record the one-time `/cache/sccache`
   deletion during its approved maintenance window.
5. Observe, then enable, recurring sccache guarding and malformed run-root
   debris cleanup.
6. Observe, then enable, warm-base idle eviction; only after that gate,
   observe and enable pressure eviction.
7. Keep fingerprint deletion disabled/dry-run-only and last, pending w06b.

## Scope, invariants, and ownership

| Path/component | Owner | Invariant / boundary |
| --- | --- | --- |
| Zot catalog retention | release operator | Only `djinn-image-*` repositories are in the Zot policy; the server startup preflight checks selected catalog images before destructive retention. |
| `/cache/sccache` | cache operator | `SCCACHE_DIR` is a namespaced compatibility fallback (`/cache/sccache/<project_id>`); task and warm builds must not depend on it. The recurring guard evaluates the parent directory as one candidate. |
| `/cache/cargo-target-runs` | coordinator | A task run writes only `/cache/cargo-target-runs/<task_run_id>`; a warm base remains `/cache/cargo-target/<project_id>`. UUID run directories are removed by worker/host teardown or orphan sweep; malformed directories and loose files are separately age-gated debris. |
| `/cache/cargo-target/<project_id>` | coordinator | Idle and pressure cleanup evict whole UUID-named warm bases only after activity, in-flight warm-job, lock, grace, and path-under-root checks. |
| Cargo fingerprints | w06b safety gate owner | No fingerprint deletion is enabled by this runbook. Fingerprints are copied into private run targets and deletion remains disabled/dry-run-only until w06b lands its empirical safety and implementation gate. |

The release operator owns Helm changes and the decision to enter `delete`
mode. The cache operator owns the one-time `/cache/sccache` deletion,
telemetry review, and the rollout record. The coordinator is the recurring
executor; it runs cargo-run debris, sccache, warm-idle, and warm-pressure
passes from `sweep_stale_resources`.

**Universal stop condition:** stop before the next destructive stage, restore
dry-run/disabled settings as applicable, and investigate if a selected image
is unsafe or unavailable, any cleanup `error` occurs, a live task/warm pod has
a nonempty `RUSTC_WRAPPER`, seed fallbacks unexpectedly rise, or the observed
candidate/path/byte evidence differs from the intended scope. Never delete a
warm base or run directory manually as a substitute for these guarded paths.

## Repository-defined controls and bounded evidence

The coordinator reads these environment variables at process construction.
For a direct binary, `DJINN_CACHE_CLEANUP_MODE` accepts `dry_run` and `delete`;
an unset or invalid value falls back to `dry_run`. In the shipped Helm chart,
the separately rendered `cacheCleanup.mode` default is `delete`; use the
warning's explicit Helm override for a dry-run observation rollout.

| Control | Default | Purpose |
| --- | --- | --- |
| `DJINN_CACHE_CLEANUP_MODE` | `dry_run` | Global destructive kill switch for sccache, debris, idle, and pressure cleanup. |
| `DJINN_CACHE_CLEANUP_SCCACHE_ENABLED` | `true` | Enables the recurring `/cache/sccache` guard. |
| `DJINN_CACHE_CLEANUP_SCCACHE_MAX_AGE_HOURS` | `24` | Stale threshold for the parent `/cache/sccache` directory. |
| `DJINN_CACHE_CLEANUP_CARGO_DEBRIS_ENABLED` | `true` | Enables malformed run-root directory and loose-file cleanup. |
| `DJINN_CACHE_CLEANUP_CARGO_DEBRIS_MAX_AGE_DAYS` | `7` | Age threshold for non-UUID directories and loose files; `0` disables debris cleanup. |
| `DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS` | `14` | Whole warm-base idle retention period. |
| `DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS` | `300` | Fresh-activity/grace protection for a warm base. |
| `DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO` | `0.15` | Free-space ratio below which pressure planning starts. |
| `DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO` | `0.25` | Free-space ratio at which pressure deletion stops. |
| `DJINN_CACHE_CLEANUP_WARM_PROFILE_MIN_IDLE_HOURS` | `24` | Profile-idle minimum for pressure planning; `0` makes a profile immediately eligible. |
| `DJINN_CARGO_TARGET_RUNS_MAX_DIRS` | `64` | Per-run target-directory count cap; `0` disables the count cap only. |
| `DJINN_CARGO_TARGET_RUNS_MAX_BYTES` | `8589934592` | Per-run allocated-byte cap (8 GiB); `0` disables the byte cap only. |

`low_free_ratio` and `high_free_ratio` must be in `[0, 1)` and low must not
exceed high. Invalid values use defaults; a low value above high is capped to
high. Use the actual `djinn-server` Deployment name in the commands below.
The chart exposes `cacheCleanup.mode` for the coordinator mode. Use Helm for
that mode (including the explicit dry-run override above), not a post-render
`kubectl set env` mutation that an upgrade can overwrite. The remaining
direct-binary tuning variables may be supplied through the approved chart/env
configuration mechanism and must be recorded. See the focused
[diagnosis runbook](SHARED_CACHE_CLEANUP_RUNBOOK.md) for strict unsigned-decimal
fallback, independent cap-disable semantics, pressure outcomes, and lock/PVC
capability gates.

```bash
export NS=djinn
export SERVER_DEPLOY=djinn-server

# Conservative, explicit baseline: report candidates and retain data.
kubectl -n "$NS" set env "deploy/$SERVER_DEPLOY" \
  DJINN_CACHE_CLEANUP_MODE=dry_run \
  DJINN_CACHE_CLEANUP_SCCACHE_ENABLED=true \
  DJINN_CACHE_CLEANUP_SCCACHE_MAX_AGE_HOURS=24 \
  DJINN_CACHE_CLEANUP_CARGO_DEBRIS_ENABLED=true \
  DJINN_CACHE_CLEANUP_CARGO_DEBRIS_MAX_AGE_DAYS=7 \
  DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS=14 \
  DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS=300 \
  DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO=0.15 \
  DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO=0.25
kubectl -n "$NS" rollout status "deploy/$SERVER_DEPLOY"
```

The bounded Prometheus series are deliberately only:

- `djinn_cache_cleanup_total{component,outcome,mode}`
- `djinn_cache_cleanup_candidates_total{component,mode}`
- `djinn_cache_cleanup_reclaimed_bytes_total{component,mode}`

Components are `sccache`, `cargo_target_runs`, and `cargo_warm_base`; modes are
`dry_run` and `delete`. Outcomes include `deleted`, `skipped`, `retained`,
`error`, `dry_run`, `uuid_orphan_deleted`, `malformed_dir_deleted`,
`loose_file_deleted`, `retained_fresh_malformed`, `retained_non_utf8`,
`retained_young`, `retained_active`, and `retained_lock_busy`. Paths and
project IDs are structured-log fields, not metric labels.

Use these PromQL examples against the existing metrics scrape target:

```promql
sum by (component, mode) (increase(djinn_cache_cleanup_candidates_total[30m]))
sum by (component, outcome, mode) (increase(djinn_cache_cleanup_total[30m]))
sum by (component, mode) (increase(djinn_cache_cleanup_reclaimed_bytes_total[30m]))
sum by (component) (increase(djinn_cache_cleanup_total{outcome="error"}[30m]))
```

For every stage, save the rendered Deployment environment, Helm values or
command used, time window, the three query outputs, relevant structured logs,
operator, approval, and rollback decision in the change record.

## Stage 0 — Zot dry-run and selected-image preflight

Zot is disabled by default. Its exact values are
`imagePipeline.zot.retention.enabled`, `.dryRun`, `.deleteUntagged`, and
`.newestTags`. With retention enabled, Zot renders a policy for
`djinn-image-*`, keeps `newestTags`, optionally removes untagged manifests,
and has `gcDelay: "1h"` and `gcInterval: "24h"`. The chart rejects destructive
retention unless both `imagePipeline.enabled` and `imagePipeline.zot.enabled`
are true.

1. Render/review the dry-run policy first; use a conservative existing default
   of five retained tags unless the approved change says otherwise.

   ```bash
   helm template "$SERVER_DEPLOY" deploy/helm/djinn -n "$NS" \
     --set imagePipeline.enabled=true \
     --set imagePipeline.zot.enabled=true \
     --set imagePipeline.zot.retention.enabled=true \
     --set imagePipeline.zot.retention.dryRun=true \
     --set imagePipeline.zot.retention.newestTags=5 \
     --set imagePipeline.zot.retention.deleteUntagged=true \
     | grep -E 'retention|dryRun|newest|deleteUntagged|gcInterval|djinn-image'
   ```

2. Deploy those values through the approved release mechanism, then wait for
   the server rollout. Startup runs the selected-image preflight whenever
   retention is enabled. It lists selected catalog images, fetches Zot state,
   and produces an advisory report in dry-run. A Zot fetch/database failure is
   a stop condition even before destructive retention.

3. Observe server and Zot logs over at least one Zot GC interval; retain the
   preflight report and evidence that selected images are pullable. Query the
   actual container names for the release rather than assuming one:

   ```bash
   kubectl -n "$NS" logs "deploy/$SERVER_DEPLOY" --all-containers --since=2h \
     | grep -Ei 'retention|preflight|selected image|zot'
   kubectl -n "$NS" get configmap "${SERVER_DEPLOY}-zot-config" \
     -o jsonpath='{.data.config\.json}'
   ```

**Gate to enable Zot deletion:** dry-run preflight/report shows every selected
image remains pullable by retained tag or digest; startup succeeds; catalog
scope is only `djinn-image-*`; and the operator has observed the scheduled Zot
retention/GC behavior. If destructive preflight finds an unsafe selected image,
it fails closed with `destructive retention blocked: ... selected image(s)
cannot be proven pullable after retention`; do not flip `.dryRun=false`.

**Enable and rollback:** only after the gate, change
`imagePipeline.zot.retention.dryRun=false` while leaving the same reviewed
`newestTags` and `deleteUntagged` settings. Roll back by setting `.dryRun=true`
(or `enabled=false` to remove the policy) and redeploy; restore a needed image
by rebuilding/publishing it and reselecting only through the normal image
control path, then rerun preflight. Do not claim a registry GC can be undone.

## Stage 1 — prove build pods do not rely on sccache

Before any `/cache/sccache` action, verify one current task-run pod and one
current warm Job pod. Both repository-generated environment shapes set
`CARGO_INCREMENTAL=1` and **clear** `RUSTC_WRAPPER=""`; `SCCACHE_DIR` may exist
as the namespaced compatibility fallback and is not proof that sccache is in
use.

```bash
export TASK_POD=<current-task-run-pod>
export WARM_POD=<current-warm-job-pod>
for pod in "$TASK_POD" "$WARM_POD"; do
  kubectl -n "$NS" exec "$pod" -- sh -lc \
    'printf "pod=%s CARGO_TARGET_DIR=%s CARGO_INCREMENTAL=%s RUSTC_WRAPPER=<%s> SCCACHE_DIR=%s\\n" \
      "$HOSTNAME" "$CARGO_TARGET_DIR" "$CARGO_INCREMENTAL" "$RUSTC_WRAPPER" "$SCCACHE_DIR"'
done
```

**Gate:** each output has `CARGO_INCREMENTAL=1` and `RUSTC_WRAPPER=<>`.
The task path must be `/cache/cargo-target-runs/<task_run_id>`; the warm path
may be `/cache/cargo-target/<project_id>`. Stop on a nonempty wrapper, correct
the Pod spec/config rollout, and rerun this gate before touching sccache.

## Stage 2 — operator-owned one-time `/cache/sccache` deletion

This is intentionally not the recurring guard. The cache operator must obtain
an explicit approved maintenance window after Stage 1, capture a size/listing,
and record the command result. Do **not** run this while build pods that might
explicitly invoke sccache are active. Use a purpose-built, approved PVC-mounted
maintenance pod; the following command is the deletion action to record, not a
claim that it has been executed by this document:

```bash
# In the approved maintenance pod where /cache is the shared cache PVC:
du -sh /cache/sccache
find /cache/sccache -mindepth 1 -maxdepth 2 -printf '%TY-%Tm-%TdT%TH:%TM:%TS %s %p\n' | sort > /var/tmp/sccache-before.txt
rm -rf -- /cache/sccache
```

**Gate/stop:** the before record exists, Stage 1 was clean, and an operator has
confirmed the maintenance window. Stop if the mounted path is not exactly
`/cache/sccache`, the mount is not the intended shared PVC, or any build pod
is still a possible writer. **Rollback/rebuild:** deletion cannot be reversed;
remove the maintenance pod and allow the next explicitly-invoked sccache user
to rebuild its namespaced cache. Djinn warm/task-run performance recovery is
through normal warm-base rebuilds, not restoration of copied sccache files.

## Stage 3 — recurring sccache guard and run-root debris cleanup

Keep `DJINN_CACHE_CLEANUP_MODE=dry_run` for this observation stage. The
sccache guard logs `path`, `size_bytes`, `latest_mtime_secs`, `age_hours`, and
`mode` on `CoordinatorActor: sccache guard candidate`; a stale candidate logs
`would delete stale directory`. It deletes the whole parent only when its
latest recursive mtime meets `SCCACHE_MAX_AGE_HOURS`. A fresh directory is
retained and warns of a possible new writer.

Malformed run-root cleanup applies only to non-UUID directories and loose
files older than `CARGO_DEBRIS_MAX_AGE_DAYS`; valid UUID run directories retain
the UUID lifecycle described in the companion guide. Dry-run logs include
`path`, `entry_kind`, `size_bytes`, `age_secs`, `threshold_secs`, `mode`, and
`cleanup_outcome`; a fresh malformed entry is retained, and non-UTF8 names are
always retained.

```bash
kubectl -n "$NS" logs "deploy/$SERVER_DEPLOY" --since=2h \
  | grep -E 'sccache guard|cargo target run-dir sweep|stale malformed|stale loose|retaining fresh malformed'
```

**Gate to enter delete mode:** bounded metrics show expected `sccache` and
`cargo_target_runs` candidates in `dry_run`, structured logs show only the
intended stale paths, no unexpected fresh-writer warning, and no `error`
outcomes. Then set only the global switch and observe one maintenance interval:

```bash
kubectl -n "$NS" set env "deploy/$SERVER_DEPLOY" DJINN_CACHE_CLEANUP_MODE=delete
kubectl -n "$NS" rollout status "deploy/$SERVER_DEPLOY"
```

Expected delete evidence is `cleanup_outcome="deleted"` and reclaimed-byte
increments for sccache; debris has `malformed_dir_deleted` or
`loose_file_deleted` plus `size_bytes`. **Rollback:** immediately set
`DJINN_CACHE_CLEANUP_MODE=dry_run`; to stop an individual recurring path also
set its `*_ENABLED=false`. Deleted cache/run debris is rebuilt by normal warm
or task execution; never recreate a deleted UUID run directory manually.

## Stage 4 — warm-base idle eviction, then pressure eviction

Do not enter this stage until Stages 0–3 have clean evidence. Both warm paths
operate on real UUID-named immediate children of `/cache/cargo-target` and
fail closed for activity, warm-job, lock, filesystem, or path-safety failures.
They keep active task runs, in-flight warm jobs, young/grace-period bases, and
lock-busy bases. Dry-run does not create the per-base flock lock file.

1. **Idle dry-run first.** Keep mode `dry_run`; use the conservative defaults
   (14 days idle, 300 seconds grace) unless an approved change selects a less
   aggressive retention. Observe `warm-base idle GC would delete idle base`
   (`project_id`, `size_bytes`, `mode`) and the completion fields `component`,
   `mode`, `deleted`, `dry_run`, `retained`, `reclaimed_bytes`, and
   `projected_bytes`.
2. **Idle enablement gate.** Candidates must be expected idle bases; retained
   reasons must show safety guards working; `projected_bytes` must fit the
   approved blast radius; no cleanup errors or seed-health regression. Switch
   global mode to `delete` for one observed interval. A successful deletion
   logs `warm-base idle GC deleted idle base` with `project_id` and
   `size_bytes`.
3. **Pressure dry-run follows idle.** Pressure starts only when available
   space is below `WARM_BASE_LOW_FREE_RATIO` and stops after reaching
   `WARM_BASE_HIGH_FREE_RATIO`. Observe `warm-base pressure GC completed` with
   `component`, `mode`, `deleted`, `dry_run`, `retained`,
   `retained_outcomes`, `reclaimed_bytes`, `projected_bytes`,
   `reached_high_watermark`, and `remeasurement_failed`.
4. **Pressure enablement gate.** Dry-run must select an expected ordered
   prefix and report acceptable projected bytes; the low/high watermarks must
   have been reviewed; `remeasurement_failed` must be false. Only then keep
   `delete` for a single observed pressure interval. Stop immediately if
   remeasurement fails, errors rise, or the high-watermark behavior differs
   from evidence.

```bash
kubectl -n "$NS" logs "deploy/$SERVER_DEPLOY" --since=2h \
  | grep -E 'warm-base (idle|pressure) GC|cargo cache health'
# Seed health is a separate bounded signal used to catch a rebuild regression.
# The structured line has project_id, seed_hit_rate, cold_fallback_count,
# and warm_base_age_seconds.
```

**Warm rollback/rebuild:** set global mode back to `dry_run` (or restore the
previous conservative retention/watermarks), wait for rollout, and record the
last completion line and metric deltas. Whole-base deletion is irreversible;
the supported rebuild is the normal warm job for that project, after which task
runs seed private directories again. Do not copy a warm base from another
project, because bases are project namespaced and Cargo fingerprints include
build configuration.

## Fingerprint-last hold

Fingerprint cleanup has no rollout command, config knob, or deletion procedure
in this artifact. The only current fingerprint behavior relevant here is that
private-target seeding copies fingerprint metadata while skipping incremental
content. Keep fingerprint deletion disabled (or dry-run-only if w06b supplies
an explicitly reviewed dry-run implementation) **last**, pending w06b's
empirical safety and implementation gate. Do not treat warm-idle/pressure
whole-base eviction as authorization to add a fingerprint sweep.

## Completion checklist

The cache operator closes this runbook only after recording all of the
following, without asserting that production actions happened merely because
the document exists. For the mechanically checkable per-component confirmation
artifact with owner, mode, expected bounded evidence, enable/stop decision,
rollback action, and explicit operator signature/record fields, use the
[Shared-cache cleanup rollout confirmation checklist](SHARED_CACHE_CLEANUP_CONFIRMATION_CHECKLIST.md).
That checklist is validated by `scripts/check-shared-cache-rollout.sh`.

- [ ] Zot Helm render, dry-run preflight report, selected-image pullability,
  and retention/GC observation; destructive Zot was either left dry-run or
  separately approved and observed.
- [ ] Task and warm pod environment evidence proves `RUSTC_WRAPPER=""` and
  `CARGO_INCREMENTAL=1`.
- [ ] The one-time `/cache/sccache` deletion has an operator, approval,
  maintenance window, pre-delete inventory, exact command result, and rebuild
  observation — distinct from the recurring guard.
- [ ] Dry-run and delete evidence (when approved) exists for sccache and
  cargo-target-runs debris, including candidates, outcomes, and bytes.
- [ ] Idle then pressure warm-base evidence includes projected/reclaimed bytes,
  retention outcomes, watermarks, and seed-health review.
- [ ] Fingerprint deletion remains disabled/dry-run-only pending w06b; no
  implementation is claimed or duplicated here.
- [ ] Any stop/rollback/rebuild action is recorded with its observed outcome.
