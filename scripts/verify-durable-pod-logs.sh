#!/usr/bin/env bash
# Composable, log-only acceptance slice for durable pod logs.
# This intentionally does not include incident monitoring or runbook contracts.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SERVER_ROOT="$REPO_ROOT/server"
RETRIEVAL_FIXTURE="$SCRIPT_DIR/test-djinn-observability-logs.sh"
RUNTIME_FIXTURE="$SCRIPT_DIR/test-djinn-log-rotator-runtime.sh"
COLLECTOR_CONTRACT="$REPO_ROOT/deploy/helm/djinn/tests/log-collector-contract.sh"
COLLECTOR_DELIVERY="$REPO_ROOT/deploy/helm/djinn/tests/log-collector-delivery.sh"
CURRENT_STAGE="startup"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

on_error() {
    local status=$?
    printf "FAIL: durable pod-log stage failed: %s (exit %d)\n" "$CURRENT_STAGE" "$status" >&2
    exit "$status"
}
trap on_error ERR

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "required tool is missing: $1"
}

require_fixture() {
    [ -x "$1" ] || fail "required executable fixture is missing: $1"
}

run_stage() {
    CURRENT_STAGE=$1
    shift
    printf '==> durable pod logs: %s\n' "$CURRENT_STAGE"
    "$@"
}

# Validate prerequisites here so an invocation from any directory reports a
# missing executable as a clear verifier failure instead of an opaque child error.
for tool in bash cargo curl gzip helm python3 vector; do
    require_tool "$tool"
done
for fixture in "$RETRIEVAL_FIXTURE" "$RUNTIME_FIXTURE" "$COLLECTOR_CONTRACT" "$COLLECTOR_DELIVERY"; do
    require_fixture "$fixture"
done

cd "$SERVER_ROOT"
# The store suite contains the named log_store::seven_day_boundary capacity test
# alongside exact quota, reserve, recovery, and complete-line store coverage.
run_stage 'Rust store and capacity suite (includes log_store::seven_day_boundary)' \
    cargo test -p djinn-log-rotator --test store
run_stage 'Rust localhost HTTP ingest, health, metrics, and reserve suite' \
    cargo test -p djinn-log-rotator --lib http::tests

cd "$REPO_ROOT"
run_stage 'active and gzip retained-log retrieval fixture' \
    bash "$RETRIEVAL_FIXTURE"
run_stage 'rendered Helm and Vector collector contract' \
    bash "$COLLECTOR_CONTRACT"
run_stage 'real rotator process restart and retrieval fixture' \
    bash "$RUNTIME_FIXTURE"
run_stage 'rendered Vector delivery and collector/rotator restart fixture' \
    bash "$COLLECTOR_DELIVERY"

printf 'PASS: durable pod-log verifier\n'
