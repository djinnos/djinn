#!/usr/bin/env bash
# Read-only aggregate acceptance gate for the shipped cache-cleanup contract.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MANIFEST_REL="scripts/fixtures/cache-cleanup/manifest.sha256"
MANIFEST="$REPO_ROOT/$MANIFEST_REL"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "required tool '$1' is not installed"
}

[[ "$(uname -s)" == "Linux" ]] || fail "cache-cleanup verification requires Linux"
for tool in sha256sum helm python3 cargo git; do
    require_tool "$tool"
done
cd "$REPO_ROOT"

[[ -f "$MANIFEST" ]] || fail "missing required manifest: $MANIFEST_REL"
[[ -s "$MANIFEST" ]] || fail "required manifest is empty: $MANIFEST_REL"

# These are intentional named inventory members, rather than merely a glob. They
# make an omission/rename fail before a test harness can accidentally skip it.
required_inventory=(
    "deploy/helm/djinn/tests/cache-cleanup-render.sh"
    "deploy/helm/djinn/tests/fixtures/cache-cleanup/invalid.env"
    "docs/SHARED_CACHE_CLEANUP_ROLLOUT.md"
    "docs/SHARED_CACHE_CLEANUP_RUNBOOK.md"
    "server/crates/djinn-coordinator/src/cargo_warm_base_gc/tests/pressure_execution.rs"
    "server/crates/djinn-coordinator/tests/fixtures/cache_cleanup/three_rung_pressure.json"
    "server/crates/djinn-core/tests/fixtures/cargo_target_runs/conjunctive_both_caps/expected.json"
    "server/crates/djinn-core/tests/fixtures/cargo_target_runs/conjunctive_both_caps/scenario.json"
    "server/crates/djinn-telemetry/tests/fixtures/cache_cleanup/expected_metrics.json"
)

while IFS=' ' read -r digest path; do
    [[ "$digest" =~ ^[[:xdigit:]]{64}$ ]] || fail "malformed SHA-256 digest in $MANIFEST_REL"
    [[ -n "${path:-}" && "$path" != /* && "$path" != *".."* ]] || fail "unsafe manifest path: ${path:-<empty>}"
    [[ -f "$path" ]] || fail "manifest inventory member is missing: $path"
    git ls-files --error-unmatch -- "$path" >/dev/null 2>&1 || fail "manifest inventory member is not tracked: $path"
done < "$MANIFEST"

for path in "${required_inventory[@]}"; do
    grep -qE "^[[:xdigit:]]{64}  ${path//./\\.}$" "$MANIFEST" || fail "required inventory member is absent: $path"
done

# Validate before any behavioral command. sha256sum's --check also reports a
# renamed/missing member rather than treating it as a skipped fixture.
sha256sum --check --status "$MANIFEST" || fail "committed cache-cleanup manifest does not match"

scratch="$(mktemp -d "${TMPDIR:-/var/tmp}/verify-cache-cleanup.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
baseline_hashes="$scratch/baseline.sha256"
baseline_status="$scratch/baseline.status"
awk '{print $2}' "$MANIFEST" | while IFS= read -r path; do sha256sum -- "$path"; done > "$baseline_hashes"
git status --porcelain -- $(awk '{print $2}' "$MANIFEST") > "$baseline_status"

run_check() {
    printf '\n=== %s ===\n' "$1"
    shift
    "$@"
}

run_cargo_filtered_check() {
    # Cargo treats a filter that selects no tests as success. Every focused
    # Rust subcheck must prove it ran at least one test, not merely built.
    local cargo_log="$scratch/cargo-filtered-$RANDOM.log"
    local status
    set +e
    (cd server && cargo test "$@") >"$cargo_log" 2>&1
    status=$?
    set -e
    cat "$cargo_log"
    if [[ $status -ne 0 ]]; then
        return "$status"
    fi
    grep -Eq 'test result: ok\. [1-9][0-9]* passed;' "$cargo_log" || {
        printf 'FAIL: focused Cargo test selected no passing tests: %s\n' "$*" >&2
        return 1
    }
}

run_check "exact Helm cache-cleanup render contract" bash deploy/helm/djinn/tests/cache-cleanup-render.sh
run_check "three-rung planner/executor guard, path, capacity, and cold-rebuild fixtures" \
    run_cargo_filtered_check -p djinn-coordinator --lib frozen_
run_check "exact pressure telemetry fixture" \
    run_cargo_filtered_check -p djinn-coordinator --lib pressure_metrics_match_the_bounded_fixture_for_execution_boundaries
run_check "real warm/pressure shared-lock two-actor schedule" \
    run_cargo_filtered_check -p djinn-coordinator --lib frozen_two_actor_schedule_serializes_warm_work_and_pressure_retry
run_check "cargo-target-runs accounting and conjunctive joint-cap fixtures" \
    run_cargo_filtered_check -p djinn-core --test cargo_target_runs_fixtures cargo_target_runs_fixture_contract
run_check "cargo-target-runs Linux lstat fixture requirement" \
    run_cargo_filtered_check -p djinn-core --test cargo_target_runs_linux_required cargo_target_runs_fixture_contract_requires_linux_lstat_semantics
run_check "direct-binary unset and invalid mode fail-safe" \
    run_cargo_filtered_check -p djinn-coordinator --lib cache_cleanup_mode_from_env_value_dry_run_fallback

run_check "deterministic rollout and runbook contract" python3 - <<'PY'
from pathlib import Path
checks = {
    "docs/SHARED_CACHE_CLEANUP_ROLLOUT.md": [
        "cacheCleanup.mode=delete", "--set cacheCleanup.mode=dry_run",
        "DJINN_CACHE_CLEANUP_MODE", "fails safe to `dry_run`",
    ],
    "docs/SHARED_CACHE_CLEANUP_RUNBOOK.md": [
        "cacheCleanup.mode` defaults to `delete`", "DJINN_CACHE_CLEANUP_MODE",
        "uses the fail-safe `dry_run` mode", "conjunctive", "shared-lock",
        "0.15", "0.25", "8589934592",
    ],
}
for name, needles in checks.items():
    text = Path(name).read_text(encoding="utf-8")
    missing = [needle for needle in needles if needle not in text]
    if missing:
        raise SystemExit(f"documentation contract failed for {name}: missing {missing}")
PY

sha256sum --check --status "$MANIFEST" || fail "verification changed a manifest baseline"
current_hashes="$scratch/current.sha256"
awk '{print $2}' "$MANIFEST" | while IFS= read -r path; do sha256sum -- "$path"; done > "$current_hashes"
cmp -s "$baseline_hashes" "$current_hashes" || fail "verification changed a tracked baseline digest"
current_status="$scratch/current.status"
git status --porcelain -- $(awk '{print $2}' "$MANIFEST") > "$current_status"
cmp -s "$baseline_status" "$current_status" || fail "verification changed tracked baseline status"
printf '\nPASS: cache-cleanup acceptance verification completed without baseline mutation\n'
