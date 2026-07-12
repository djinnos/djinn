#!/bin/sh
# Self-test harness for scripts/check-shared-cache-rollout.sh.
#
# Exercises the validation guard against synthetic fixture files that this
# script creates (and tears down) under a scratch directory. Each test mutates
# one assertion category and confirms the guard fails; a final test confirms
# the guard passes against the real checked-in docs.
#
# Pure POSIX shell; no cargo, no python, no network, no Docker, no Kubernetes,
# no Zot, no Prometheus, no production filesystem access.
#
# Run from the repository root:
#
#   sh scripts/test-check-shared-cache-rollout.sh
#
# Exits 0 on success. The first failing assertion aborts the harness with a
# non-zero status, and the EXIT trap removes every fixture path.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GUARD="$SCRIPT_DIR/check-shared-cache-rollout.sh"

cleanup() {
    if [ -n "${FIXTURE_DIR:-}" ] && [ -d "$FIXTURE_DIR" ]; then
        rm -rf -- "$FIXTURE_DIR" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

if [ ! -f "$GUARD" ]; then
    printf 'FATAL: production guard not found at %s\n' "$GUARD" >&2
    exit 2
fi

PASS=0
FAIL=0
# Prefer /var/tmp (disk-backed, always writable in CI sandboxes), then the
# per-run djinn cache, then $TMPDIR. Avoid $TMPDIR=/tmp on hosts that sandbox.
FIXTURE_DIR=$(mktemp -d /var/tmp/djinn-rollout-guard-test.XXXXXX 2>/dev/null || \
              mktemp -d "$HOME/.cache/djinn/djinn-rollout-guard-test.XXXXXX" 2>/dev/null || \
              mktemp -d "${TMPDIR:-.}/djinn-rollout-guard-test.XXXXXX")
if [ ! -d "$FIXTURE_DIR" ]; then
    printf 'FATAL: could not create scratch fixture dir\n' >&2
    exit 2
fi

# Fixture doc paths inside the scratch dir.
F_RUNBOOK="$FIXTURE_DIR/SHARED_CACHE_CLEANUP_ROLLOUT.md"
F_CHECKLIST="$FIXTURE_DIR/SHARED_CACHE_CLEANUP_CONFIRMATION_CHECKLIST.md"
F_RUN_DIR="$FIXTURE_DIR/CARGO_TARGET_RUN_DIR_VALIDATION.md"
F_ZOT="$FIXTURE_DIR/zot-retention-gc-observation.md"

pass() {
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
}

fail() {
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n' "$1" >&2
    if [ -n "${2:-}" ]; then
        printf '       %s\n' "$2" >&2
    fi
}

# Run the guard against the fixture paths. Returns the guard's exit status.
# Output is captured to a log file for inspection.
run_guard() {
    label=$1
    log="$FIXTURE_DIR/$label.log.out"
    # NOTE: do not restore `set -e` here. In POSIX sh, `set -e` is a global
    # shell option, not function-scoped — restoring it inside this function
    # would cause a non-zero `return $rc` to abort the caller before it can
    # capture the exit code. The caller wraps each call in set +e / set -e.
    set +e
    env \
        SHARED_CACHE_ROLLOUT_RUNBOOK="$F_RUNBOOK" \
        SHARED_CACHE_ROLLOUT_CHECKLIST="$F_CHECKLIST" \
        SHARED_CACHE_ROLLOUT_RUN_DIR_GUIDE="$F_RUN_DIR" \
        SHARED_CACHE_ROLLOUT_ZOT_OBSERVATION="$F_ZOT" \
        sh "$GUARD" > "$log" 2>&1
    rc=$?
    return $rc
}

assert_exit() {
    label=$1
    expected=$2
    actual=$3
    log_path=$4
    if [ "$expected" -eq 0 ] && [ "$actual" -eq 0 ]; then
        pass "$label"
    elif [ "$expected" -ne 0 ] && [ "$actual" -ne 0 ]; then
        pass "$label (exit=$actual)"
    else
        fail "$label" "expected exit=$expected, got exit=$actual
output:
$(cat "$log_path")"
    fi
}

assert_output_contains() {
    label=$1
    needle=$2
    log_path=$3
    if grep -qF -- "$needle" "$log_path"; then
        pass "$label"
    else
        fail "$label" "expected output to contain '$needle'
actual output:
$(cat "$log_path")"
    fi
}

# ── Build a known-good fixture set ───────────────────────────────────
# These fixtures are minimal but satisfy every assertion the guard makes.
# Each drift test below mutates one field and expects failure.

write_good_runbook() {
    cat > "$F_RUNBOOK" <<'EOF'
# Shared-cache cleanup rollout and rollback runbook

This is a dry-run-first procedure. The companion
[CARGO_TARGET_RUN_DIR_VALIDATION.md](CARGO_TARGET_RUN_DIR_VALIDATION.md)
covers the per-task-run directory lifecycle.

## Required rollout order

## Scope, invariants, and ownership

## Repository-defined controls and bounded evidence

`DJINN_CACHE_CLEANUP_MODE` accepts dry_run and delete.
DJINN_CACHE_CLEANUP_SCCACHE_ENABLED
DJINN_CACHE_CLEANUP_SCCACHE_MAX_AGE_HOURS
DJINN_CACHE_CLEANUP_CARGO_DEBRIS_ENABLED
DJINN_CACHE_CLEANUP_CARGO_DEBRIS_MAX_AGE_DAYS
DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS
DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS
DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO
DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO

djinn_cache_cleanup_total
djinn_cache_cleanup_candidates_total
djinn_cache_cleanup_reclaimed_bytes_total

Components are sccache, cargo_target_runs, and cargo_warm_base; modes are
dry_run and delete. Outcomes include deleted, skipped, retained, error,
dry_run, uuid_orphan_deleted, malformed_dir_deleted, loose_file_deleted,
retained_fresh_malformed, retained_non_utf8, retained_young,
retained_active, and retained_lock_busy.

Universal stop condition.

```bash
kubectl -n "$NS" set env "deploy/$SERVER_DEPLOY" DJINN_CACHE_CLEANUP_MODE=dry_run
kubectl -n "$NS" rollout status "deploy/$SERVER_DEPLOY"
```

## Stage 0 — Zot dry-run and selected-image preflight

helm template with imagePipeline.zot.retention.enabled,
imagePipeline.zot.retention.dryRun, imagePipeline.zot.retention.newestTags,
imagePipeline.zot.retention.deleteUntagged. gcDelay and gcInterval.

## Stage 1 — prove build pods do not rely on sccache

## Stage 2 — operator-owned one-time `/cache/sccache` deletion

du -sh /cache/sccache
find /cache/sccache
rm -rf -- /cache/sccache

## Stage 3 — recurring sccache guard and run-root debris cleanup

## Stage 4 — warm-base idle eviction, then pressure eviction

warm-base idle GC would delete idle base
warm-base idle GC deleted idle base
warm-base pressure GC completed
projected_bytes reclaimed_bytes reached_high_watermark remeasurement_failed
retained_outcomes

## Fingerprint-last hold

pending w06b.

## Completion checklist
EOF
}

write_good_checklist() {
    cat > "$F_CHECKLIST" <<'EOF'
# Shared-cache cleanup rollout confirmation checklist

dry-run-first. Links: [SHARED_CACHE_CLEANUP_ROLLOUT.md](SHARED_CACHE_CLEANUP_ROLLOUT.md)
and [CARGO_TARGET_RUN_DIR_VALIDATION.md](CARGO_TARGET_RUN_DIR_VALIDATION.md).
Zot: zot-retention-gc-observation.md.

## Component rows

### 1. `zot_retention`

### 2. `sccache`

### 3. `cargo_target_runs_debris`

### 4. `warm_idle`

### 5. `warm_pressure`

### 6. `warm_fingerprint` (gated, last)

> Fail-safe and last. This references the w06b gate and cannot be read as
> proof that destructive fingerprint cleanup already exists or is enabled.

| Enable decision | **None.** |

Pre-delete confirmation — operator name: __________
Pre-delete confirmation — approval reference: __________
approved maintenance window: __________
post-delete rebuild observation: __________

These fields are deliberately separated from repository-automated proof.
Validated by scripts/check-shared-cache-rollout.sh.
EOF
}

write_good_run_dir() {
    cat > "$F_RUN_DIR" <<'EOF'
# Cargo target run-dir rollout validation

See SHARED_CACHE_CLEANUP_ROLLOUT.md.
EOF
}

write_good_zot() {
    cat > "$F_ZOT" <<'EOF'
# Zot Retention and GC Observation

disabled dry_run destructive
disabled advisory destructive_safe destructive_blocked fetch_error
EOF
}

printf '== running self-tests for scripts/check-shared-cache-rollout.sh ==\n'

# ── T0: known-good fixtures pass ─────────────────────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
set +e
run_guard t0_good
t0_rc=$?
set -e
assert_exit "T0 known-good fixtures pass" 0 "$t0_rc" "$FIXTURE_DIR/t0_good.log.out"

# ── T1: missing runbook heading fails ────────────────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
# Remove one required heading from the runbook.
sed -i '/^## Fingerprint-last hold$/d' "$F_RUNBOOK"
set +e
run_guard t1_missing_heading
t1_rc=$?
set -e
assert_exit "T1 missing runbook heading fails" 1 "$t1_rc" "$FIXTURE_DIR/t1_missing_heading.log.out"
assert_output_contains "T1 reports missing heading" \
    "FAIL runbook heading present: ## Fingerprint-last hold" \
    "$FIXTURE_DIR/t1_missing_heading.log.out"

# ── T2: checklist missing component row fails ───────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
# Remove the warm_idle component row heading.
sed -i '/^### 4\. `warm_idle`$/d' "$F_CHECKLIST"
set +e
run_guard t2_missing_component
t2_rc=$?
set -e
assert_exit "T2 missing checklist component row fails" 1 "$t2_rc" "$FIXTURE_DIR/t2_missing_component.log.out"
assert_output_contains "T2 reports missing component" \
    "FAIL checklist row for warm_idle" \
    "$FIXTURE_DIR/t2_missing_component.log.out"

# ── T3: wrong component order fails ─────────────────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
# Swap warm_idle and warm_pressure by renumbering: move warm_pressure before
# warm_idle. Rewrite the two headings so pressure gets a lower number.
sed -i 's/^### 4\. `warm_idle`$/### 4. `warm_pressure`/' "$F_CHECKLIST"
sed -i 's/^### 5\. `warm_pressure`$/### 5. `warm_idle`/' "$F_CHECKLIST"
set +e
run_guard t3_wrong_order
t3_rc=$?
set -e
assert_exit "T3 wrong component order fails" 1 "$t3_rc" "$FIXTURE_DIR/t3_wrong_order.log.out"
assert_output_contains "T3 reports order violation" \
    "checklist order" \
    "$FIXTURE_DIR/t3_wrong_order.log.out"

# ── T4: warm_fingerprint not last fails ─────────────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
# Move warm_fingerprint to NOT be last by adding a fake 7th component after it.
cat >> "$F_CHECKLIST" <<'EOF'

### 7. `extra_after_fingerprint`
EOF
set +e
run_guard t4_fp_not_last
t4_rc=$?
set -e
assert_exit "T4 warm_fingerprint not last fails" 1 "$t4_rc" "$FIXTURE_DIR/t4_fp_not_last.log.out"
assert_output_contains "T4 reports fingerprint not last" \
    "warm_fingerprint is the last checklist component row" \
    "$FIXTURE_DIR/t4_fp_not_last.log.out"

# ── T5: fingerprint row implying enablement fails ───────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
# Remove the fail-safe phrase so the row could be read as enablement.
sed -i 's/cannot be read as/removed phrase/' "$F_CHECKLIST"
set +e
run_guard t5_fp_enablement
t5_rc=$?
set -e
assert_exit "T5 fingerprint enablement implication fails" 1 "$t5_rc" "$FIXTURE_DIR/t5_fp_enablement.log.out"
assert_output_contains "T5 reports not-enablement phrase missing" \
    "fingerprint row is not enablement" \
    "$FIXTURE_DIR/t5_fp_enablement.log.out"

# ── T6: missing telemetry stable name fails ────────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
# Remove a telemetry component label from the runbook.
sed -i 's/cargo_warm_base/cargo_warm_DRIFT/' "$F_RUNBOOK"
set +e
run_guard t6_telemetry_drift
t6_rc=$?
set -e
assert_exit "T6 telemetry component drift fails" 1 "$t6_rc" "$FIXTURE_DIR/t6_telemetry_drift.log.out"
assert_output_contains "T6 reports telemetry component drift" \
    "runbook names telemetry component: cargo_warm_base" \
    "$FIXTURE_DIR/t6_telemetry_drift.log.out"

# ── T7: missing env var fails ──────────────────────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
sed -i '/DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO/d' "$F_RUNBOOK"
set +e
run_guard t7_env_drift
t7_rc=$?
set -e
assert_exit "T7 env var drift fails" 1 "$t7_rc" "$FIXTURE_DIR/t7_env_drift.log.out"
assert_output_contains "T7 reports env var drift" \
    "runbook references env var: DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO" \
    "$FIXTURE_DIR/t7_env_drift.log.out"

# ── T8: missing sccache deletion command fails ─────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
sed -i '/rm -rf -- \/cache\/sccache/d' "$F_RUNBOOK"
set +e
run_guard t8_sccache_cmd
t8_rc=$?
set -e
assert_exit "T8 missing sccache deletion command fails" 1 "$t8_rc" "$FIXTURE_DIR/t8_sccache_cmd.log.out"
assert_output_contains "T8 reports missing sccache command" \
    "runbook records rm -rf -- /cache/sccache" \
    "$FIXTURE_DIR/t8_sccache_cmd.log.out"

# ── T9: missing operator record field fails ────────────────────────
write_good_runbook
write_good_checklist
write_good_run_dir
write_good_zot
sed -i '/Pre-delete confirmation — operator name/d' "$F_CHECKLIST"
set +e
run_guard t9_operator_field
t9_rc=$?
set -e
assert_exit "T9 missing operator record field fails" 1 "$t9_rc" "$FIXTURE_DIR/t9_operator_field.log.out"
assert_output_contains "T9 reports missing operator field" \
    "checklist has pre-delete operator field" \
    "$FIXTURE_DIR/t9_operator_field.log.out"

# ── T10: missing required file fails (exit 2) ──────────────────────
rm -f "$F_ZOT"
set +e
run_guard t10_missing_file
t10_rc=$?
set -e
assert_exit "T10 missing required file fails" 2 "$t10_rc" "$FIXTURE_DIR/t10_missing_file.log.out"

# ── T11: guard passes against the real checked-in docs ─────────────
# This confirms the guard is not just passing against synthetic fixtures but
# also against the actual repository artifacts it is meant to protect.
set +e
sh "$GUARD" > "$FIXTURE_DIR/t11_real.log.out" 2>&1
t11_rc=$?
set -e
assert_exit "T11 real checked-in docs pass" 0 "$t11_rc" "$FIXTURE_DIR/t11_real.log.out"

# ── summary ─────────────────────────────────────────────────────────
printf -- '------------------------------------------\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
exit 0
