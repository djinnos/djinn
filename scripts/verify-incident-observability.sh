#!/usr/bin/env bash
# Hermetic umbrella acceptance verifier for incident observability.
#
# This composes the durable pod-log slice and the focused runtime, chart, and
# runbook contracts. It intentionally requires no cluster, mounted disk,
# Kubernetes Secret, network service, or external paging provider.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SERVER_ROOT="$REPO_ROOT/server"
DURABLE_LOG_VERIFIER="$SCRIPT_DIR/verify-durable-pod-logs.sh"
CHART_DIR="$REPO_ROOT/deploy/helm/djinn"
RULE_TESTS="deploy/helm/djinn/tests/incident-observability-rules.yml"
HELM_CONTRACT="$CHART_DIR/tests/incident-observability-contract.sh"
DEADMAN_FIXTURE="$CHART_DIR/tests/deadman-fixture.sh"
RENDER_FIXTURE="$CHART_DIR/tests/fixtures/incident-observability-schema-accept.yaml"
RUNBOOK="$REPO_ROOT/server/docs/runbooks/incident-observability.md"
CHART_README="$CHART_DIR/README.md"
CURRENT_STAGE="startup"

fail() {
    printf 'FAIL: incident observability stage failed: %s: %s\n' "$CURRENT_STAGE" "$*" >&2
    exit 1
}

on_error() {
    local status=$?
    printf 'FAIL: incident observability stage failed: %s (exit %d)\n' "$CURRENT_STAGE" "$status" >&2
    exit "$status"
}
trap on_error ERR

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "required tool is missing: $1"
}

require_fixture() {
    [ -x "$1" ] || fail "required executable fixture is missing: $1"
}

require_file() {
    [ -f "$1" ] || fail "required fixture is missing: $1"
}

run_stage() {
    CURRENT_STAGE=$1
    shift
    printf '==> incident observability: %s\n' "$CURRENT_STAGE"
    "$@"
}

# Check every direct and composed dependency before running a child stage, so a
# missing local prerequisite fails closed with an actionable verifier error.
CURRENT_STAGE="prerequisites"
for tool in bash cargo curl gzip helm openssl promtool python3 vector; do
    require_tool "$tool"
done
for fixture in "$DURABLE_LOG_VERIFIER" "$HELM_CONTRACT" "$DEADMAN_FIXTURE"; do
    require_fixture "$fixture"
done
for fixture in "$RULE_TESTS" "$RENDER_FIXTURE" "$RUNBOOK" "$CHART_README"; do
    require_file "$fixture"
done

cd "$REPO_ROOT"
run_stage 'durable pod-log acceptance slice' \
    bash "$DURABLE_LOG_VERIFIER"

cd "$SERVER_ROOT"
run_stage 'log_store::seven_day_boundary' \
    cargo test -p djinn-log-rotator --test store log_store::seven_day_boundary -- --exact
run_stage 'panic_capture::chained_large_hook' \
    cargo test -p djinn-telemetry --lib panic_capture::chained_large_hook -- --exact
run_stage 'job_retention::all_boundaries' \
    cargo test -p djinn-core --lib job_retention::retention_policy::all_boundaries -- --exact
run_stage 'attempt_evidence::v2_contract' \
    cargo test -p djinn-k8s --lib infra_death_log_tail::tests::attempt_evidence::v2_contract -- --exact

cd "$REPO_ROOT"
run_stage 'Prometheus incident-observability rules' \
    promtool test rules deploy/helm/djinn/tests/incident-observability-rules.yml
run_stage 'deadman_fixture::watchdog_absence' \
    bash "$DEADMAN_FIXTURE"
run_stage 'helm_contract::incident_observability' \
    bash "$HELM_CONTRACT"
run_stage 'Helm lint' \
    helm lint "$CHART_DIR"
run_stage 'Helm enabled incident-observability render' \
    helm template incident-observability "$CHART_DIR" --is-upgrade -f "$RENDER_FIXTURE"
run_stage 'incident-observability runbook contract' \
    python3 - "$RUNBOOK" "$CHART_README" <<'PY'
import sys
from pathlib import Path

runbook = Path(sys.argv[1]).read_text(encoding="utf-8")
readme = Path(sys.argv[2]).read_text(encoding="utf-8")

required_runbook_sections = (
    "## Prerequisites and production gates",
    "## Safe retained-log retrieval",
    "## Triage alerts and store pressure",
    "## Muted-to-live canary",
    "## Component rollback and impairment record",
    "DjinnDispatchWithoutCompletion",
    "DjinnServerMemoryPressure",
    "DjinnServerMetricsMissing",
    "DjinnLogStoreUnavailable",
    "DjinnLogRotatorMissing",
    "Watchdog",
)
for expected in required_runbook_sections:
    if expected not in runbook:
        raise SystemExit(f"runbook contract missing: {expected}")

if "incident-observability-contract.sh" not in readme:
    raise SystemExit("Helm README does not link the incident-observability contract")
if "incident-observability.md" not in readme:
    raise SystemExit("Helm README does not link the incident-observability runbook")
PY

printf 'PASS: incident observability verifier\n'
