# Shared-cache cleanup diagnosis runbook

This focused operational reference covers proposal `y6a0` in roadmap
[`design/j41i-roadmap`](../design/j41i-roadmap). It explains the shipped
contract and diagnosis procedure; it does **not** assert that an external
rollout was performed. Record observed cluster evidence and approvals in the
[shared-cache cleanup confirmation checklist](SHARED_CACHE_CLEANUP_CONFIRMATION_CHECKLIST.md).
For staged Zot, sccache, and warm-base rollout gates, see the companion
[rollout document](SHARED_CACHE_CLEANUP_ROLLOUT.md).

## Mode contract, dry-run override, and rollback

> **Destructive default:** Helm `cacheCleanup.mode` defaults to `delete` for
> shipped installs and upgrades. Treat every install or upgrade without an
> explicit override as destructive cleanup authorization.

Use an explicit Helm override before an observation rollout:

```bash
export NS=djinn
export RELEASE=djinn
helm upgrade --install "$RELEASE" deploy/helm/djinn --namespace "$NS" \
  --create-namespace --set cacheCleanup.mode=dry_run --wait
```

Return an existing release to dry-run immediately if a stop condition occurs:

```bash
helm upgrade "$RELEASE" deploy/helm/djinn --namespace "$NS" \
  --reuse-values --set cacheCleanup.mode=dry_run --wait
```

If the approved rollback is the previous complete release configuration, use
the recorded revision rather than guessing its values:

```bash
helm history "$RELEASE" --namespace "$NS"
helm rollback "$RELEASE" <previous-revision> --namespace "$NS" --wait
```

This Helm contract is intentionally distinct from direct-binary behavior. A
direct `djinn-server` process with `DJINN_CACHE_CLEANUP_MODE` unset or invalid
uses the fail-safe `dry_run` mode. The direct parser accepts `delete` as the
destructive value; invalid input is not permission to delete. Helm supplies a
valid environment value from `cacheCleanup.mode`, and its shipped default is
therefore `delete`.

## Normative defaults and parser behavior

| Control | Normative default | Exact semantics |
| --- | ---: | --- |
| `cacheCleanup.mode` (Helm) | `delete` | Shipped install/upgrade default. Override explicitly with `--set cacheCleanup.mode=dry_run` for observation. |
| `DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO` | `0.15` | Pressure starts below this free-space ratio. |
| `DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO` | `0.25` | Pressure stops at this free-space ratio. |
| `DJINN_CACHE_CLEANUP_WARM_PROFILE_MIN_IDLE_HOURS` | `24` | A profile becomes pressure-eligible after this project-idle period; `0` means immediate eligibility. Artifact mtimes do not prove profile idleness. |
| `DJINN_CARGO_TARGET_RUNS_MAX_DIRS` | `64` | Maximum run directories. `0` disables **count only**. |
| `DJINN_CARGO_TARGET_RUNS_MAX_BYTES` | `8589934592` | Maximum allocated bytes (8 GiB). `0` disables **bytes only**. |

The two cargo-target-runs caps are conjunctive when both are nonzero: cleanup
continues until both enabled limits are satisfied. Disabling one does not
disable the other. A protected entry or failed inventory/removal can leave an
overage; see [Cargo-target-runs overage](#cargo-target-runs-overage).

`WARM_PROFILE_MIN_IDLE_HOURS`, `MAX_DIRS`, and `MAX_BYTES` use strict unsigned
decimal parsing: only nonempty ASCII decimal digits are valid. Whitespace,
signs (including `+1`), suffixes, and overflow are invalid. Invalid values fall
back to their respective normative defaults. `WARM_PROFILE_MIN_IDLE_HOURS`
silently falls back to `24`; it does not emit an invalid-resolution diagnostic.
The cargo-target-runs caps (`MAX_DIRS` and `MAX_BYTES`) instead emit bounded
invalid-resolution diagnostics. Do not treat an invalid string as zero,
disablement, or proof that a production configuration was exercised. Zero is
valid only with the exact semantics in the table.

## Pressure reclamation: order, bounds, and stop outcomes

Pressure begins only below low (`0.15`) and attempts to reach high (`0.25`) from
one immutable inventory snapshot. Its ordered three rungs are:

1. **Incremental:** remove disposable incremental state from eligible profiles.
2. **Stale profile:** remove an eligible stale profile bundle.
3. **Whole base:** remove an eligible project warm base.

The executor rechecks safety under the common project lock, remeasures capacity
immediately before an attempt and after every successful deletion, and stops on
a capacity error. Dry-run is immutable-plan reporting only: it does not open or
create a lock, recheck, remeasure, or mutate.

| Outcome | Meaning | Operator action |
| --- | --- | --- |
| `reached_high` | The high watermark was reached (including before another attempted deletion). No further unit is deleted in that sweep. | Record the completion; do not expect the remaining plan prefix to run. |
| `remeasure_failed` | Capacity measurement failed before/after an attempt. The sweep stops fail-closed; a deletion already recorded before a post-success failure remains truthful. | Stop destructive rollout, set Helm mode to `dry_run`, and diagnose filesystem/PVC capacity access. |
| retained/protected | Activity, warm-job, grace, lock, path, symlink, scan, or removal guards prevented a unit. | Treat as a safety result, not an invitation to delete manually. |
| error | A bounded execution/measurement failure occurred. | Stop and roll back to explicit dry-run before further mutation. |

Within a base, a guard, path/traversal, lock, or removal failure/skip blocks
escalation to a broader rung for that same base in that sweep. This same-base
escalation barrier preserves siblings and base metadata rather than turning a
sub-base problem into whole-base deletion. The run can also end above high if
the bounded eligible plan is exhausted or protected; that is a stop/diagnosis
state, not a reason to bypass guards.

## Cargo-target-runs overage

`/cache/cargo-target-runs/<task_run_id>` is private, ephemeral task-run state.
The coordinator inventories allocated bytes (not a `du` estimate), orders
eligible candidates deterministically, and applies the enabled directory-count
and byte caps together.

An overage with protected entries is expected when a top-level entry cannot be
safely removed: malformed names, top-level symlinks, non-directory entries, or
an incomplete nested scan are protected. An inventory, stat, read, overflow,
or removal error similarly prevents claiming that the cap was satisfied. The
diagnosis is to inspect bounded protected/error diagnostics and the expected
run-root shape, correct the producer/PVC condition, then observe another
dry-run. Do not manually remove a protected path merely to force an apparent
cap result, and do not claim that documentation or a diagnostic indicates an
external cleanup occurred.

For a normal cap change, preserve the independent semantics explicitly:

```bash
# Count is disabled; the 8 GiB byte cap remains enabled.
DJINN_CARGO_TARGET_RUNS_MAX_DIRS=0
DJINN_CARGO_TARGET_RUNS_MAX_BYTES=8589934592

# Byte cap is disabled; the 64-directory count cap remains enabled.
DJINN_CARGO_TARGET_RUNS_MAX_DIRS=64
DJINN_CARGO_TARGET_RUNS_MAX_BYTES=0
```

## Shared warm-lock and production-PVC capability gate

Warm work and pressure GC coordinate through the same advisory lock path:

```text
/cache/cargo-target/.warm-locks/<project-id>.lock
```

Before enabling destructive warm pressure on a production PVC, demonstrate in
the production-equivalent mount/capability configuration that this lock path
can be created/opened and advisory locking works across the warm worker and
coordinator actors. The gate is required because a PVC/storage-class,
permission, ownership, mount, or advisory-lock capability failure must fail
closed. A lock-open, lock-probe, or lock-acquire failure is a stop condition:
keep/restore Helm `cacheCleanup.mode=dry_run`, investigate the PVC capability,
and rerun the dry-run gate after correction. Do not substitute a different lock
path, manually remove a lock file, or assume a local filesystem result proves
production-PVC support.

## Evidence boundary and stop checklist

Repository fixtures and these instructions establish expected contracts; they
are not worker acceptance proof and cannot prove a cluster rollout. Operators
should retain the rendered Helm command/values, Helm revision, rollout status,
bounded telemetry/log outcomes, capacity observation, PVC-lock capability
result, approval, and rollback decision in the confirmation checklist.

Stop before further destructive work and restore explicit Helm dry-run on:

- `remeasure_failed`, any cleanup `error`, or unexpected capacity behavior;
- an unexpected candidate, protected/error overage, or bytes outside the
  approved scope;
- a shared-lock/PVC capability failure; or
- evidence that differs from the reviewed dry-run plan.
