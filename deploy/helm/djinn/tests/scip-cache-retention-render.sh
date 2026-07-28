#!/usr/bin/env bash
# Exact Helm render contract for the SCIP indexer cache retention bound.
# Usage: bash deploy/helm/djinn/tests/scip-cache-retention-render.sh
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
    helm template scip-cache-retention-test "$CHART_DIR" \
        --is-upgrade \
        --show-only templates/deployment-server.yaml "$@" > "$output"
}

assert_env() {
    local manifest=$1 expected_bytes=$2 expected_idle_hours=$3
    python3 - "$manifest" "$expected_bytes" "$expected_idle_hours" <<'PY'
import re
import sys

manifest, expected_bytes, expected_idle_hours = sys.argv[1:]
lines = open(manifest, encoding="utf-8").read().splitlines()


def env_value(name):
    """Value of the `- name: <name>` / `value: <v>` pair, as rendered."""
    for index, line in enumerate(lines):
        if re.match(rf"^\s*- name: {re.escape(name)}$", line):
            match = re.match(r"^\s*value: (.+)$", lines[index + 1])
            assert match, f"{name} has no rendered value"
            value = match.group(1)
            assert value.startswith('"') and value.endswith('"'), (
                f"{name} must render as a quoted literal, got {value!r}"
            )
            return value.strip('"')
    raise AssertionError(f"{name} is not rendered onto the server Deployment")


for name, expected in (
    ("DJINN_SCIP_CACHE_MAX_BYTES", expected_bytes),
    ("DJINN_SCIP_CACHE_MAX_IDLE_HOURS", expected_idle_hours),
):
    actual = env_value(name)
    assert actual == expected, f"{name}: expected {expected!r}, got {actual!r}"
    # Helm decodes YAML numbers as float64. A bare `| quote` renders the 4 GiB
    # default as "4.294967296e+09", which djinn-graph cannot parse and silently
    # drops in favour of its own default — a knob that reads as wired but is
    # not. Pin the decimal-integer form, not merely the numeric equality.
    assert re.fullmatch(r"[0-9]+", actual), (
        f"{name} must render as a decimal integer, got {actual!r}"
    )
PY
}

expect_rejected() {
    local name=$1
    shift
    echo "=== invalid $name ==="
    if render "$TMPDIR_RENDER/$name.yaml" "$@" 2>&1; then
        echo "FAIL: invalid scipCache scenario '$name' rendered successfully" >&2
        exit 1
    fi
}

echo "=== shipped defaults are a finite bound ==="
render "$TMPDIR_RENDER/defaults.yaml"
assert_env "$TMPDIR_RENDER/defaults.yaml" 4294967296 168

echo "=== operator tuning ==="
render "$TMPDIR_RENDER/tuned.yaml" \
    --set scipCache.maxBytes=1073741824 \
    --set scipCache.maxIdleHours=48
assert_env "$TMPDIR_RENDER/tuned.yaml" 1073741824 48

echo "=== explicit opt-out of a single leg ==="
render "$TMPDIR_RENDER/uncapped.yaml" --set scipCache.maxBytes=0
assert_env "$TMPDIR_RENDER/uncapped.yaml" 0 168

expect_rejected negative-bytes --set scipCache.maxBytes=-1
expect_rejected negative-idle --set scipCache.maxIdleHours=-1
expect_rejected noninteger-bytes --set-string scipCache.maxBytes=lots
expect_rejected unknown-key --set scipCache.maxGigabytes=4

echo "=== All SCIP cache retention Helm render tests passed ==="
