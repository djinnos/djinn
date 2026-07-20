#!/usr/bin/env bash
# Exact Helm render contract for graph-generation retention rollout controls.
# Usage: bash deploy/helm/djinn/tests/graph-retention-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    }
}

require_tool helm
require_tool python3
TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

render() {
    local output=$1
    shift
    helm template graph-retention-test "$CHART_DIR" \
        --is-upgrade \
        --show-only templates/configmap.yaml "$@" > "$output"
}

assert_configmap_values() {
    local manifest=$1 expected_mode=$2 expected_history_n=$3
    python3 - "$manifest" "$expected_mode" "$expected_history_n" <<'PY'
import re
import sys

manifest, expected_mode, expected_history_n = sys.argv[1:]
lines = open(manifest, encoding="utf-8").read().splitlines()

def literal(name):
    matches = [re.match(rf"^  {re.escape(name)}: (.+)$", line) for line in lines]
    values = [match.group(1) for match in matches if match]
    assert len(values) == 1, f"expected exactly one {name}, got {len(values)}"
    value = values[0]
    assert value.startswith('"') and value.endswith('"'), f"{name} must be literal and quoted"
    return value.strip('"')

assert literal("DJINN_GRAPH_RETENTION_MODE") == expected_mode
assert literal("DJINN_GRAPH_RETENTION_HISTORY_N") == expected_history_n
PY
}

expect_rejected() {
    local name=$1
    shift
    echo "=== invalid $name ==="
    if render "$TMPDIR_RENDER/$name.yaml" "$@" 2>&1; then
        echo "FAIL: invalid graphRetention scenario '$name' rendered successfully" >&2
        exit 1
    fi
}

echo "=== production-safe defaults ==="
render "$TMPDIR_RENDER/defaults.yaml"
assert_configmap_values "$TMPDIR_RENDER/defaults.yaml" dry_run 3

for mode in off dry_run delete; do
    echo "=== explicit mode: $mode ==="
    render "$TMPDIR_RENDER/$mode.yaml" --set-string "graphRetention.mode=$mode"
    assert_configmap_values "$TMPDIR_RENDER/$mode.yaml" "$mode" 3
done

echo "=== custom history N ==="
render "$TMPDIR_RENDER/custom-history.yaml" --set graphRetention.historyN=7
assert_configmap_values "$TMPDIR_RENDER/custom-history.yaml" dry_run 7

expect_rejected unknown-mode --set-string graphRetention.mode=observe
expect_rejected zero-history --set graphRetention.historyN=0
expect_rejected negative-history --set graphRetention.historyN=-1
expect_rejected oversized-history --set graphRetention.historyN=65
expect_rejected noninteger-history --set-string graphRetention.historyN=three

echo "=== All graph-retention Helm render tests passed ==="
