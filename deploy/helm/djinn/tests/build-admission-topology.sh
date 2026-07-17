#!/usr/bin/env bash
# Helm render contract for single-active build-admission enforcement.
# Usage: bash deploy/helm/djinn/tests/build-admission-topology.sh
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
    helm template build-admission-test "$CHART_DIR" \
        --show-only templates/deployment-server.yaml "$@" > "$output"
}

assert_server_contract() {
    local manifest=$1 mode=$2 cap=$3 replicas=$4 strategy=$5 surge=${6:-} unavailable=${7:-}
    python3 - "$manifest" "$mode" "$cap" "$replicas" "$strategy" "$surge" "$unavailable" <<'PY'
import re
import sys

path, expected_mode, expected_cap, expected_replicas, expected_strategy, expected_surge, expected_unavailable = sys.argv[1:]
lines = open(path, encoding="utf-8").read().splitlines()

def scalar(pattern, label):
    matches = [re.match(pattern, line) for line in lines]
    values = [m.group(1).strip('"') for m in matches if m]
    assert len(values) == 1, f"expected exactly one {label}, got {len(values)}"
    return values[0]

assert scalar(r"^  replicas: (.+)$", "server replicas") == expected_replicas
assert scalar(r"^    type: (.+)$", "server strategy type") == expected_strategy

if expected_strategy == "RollingUpdate":
    assert scalar(r"^      maxSurge: (.+)$", "maxSurge") == expected_surge
    assert scalar(r"^      maxUnavailable: (.+)$", "maxUnavailable") == expected_unavailable
else:
    assert not any(line.startswith("    rollingUpdate:") for line in lines), "Recreate must not render rollingUpdate"

def env_value(name):
    marker = f"            - name: {name}"
    locations = [i for i, line in enumerate(lines) if line == marker]
    assert len(locations) == 1, f"expected exactly one {name} env entry, got {len(locations)}"
    line = lines[locations[0] + 1]
    match = re.match(r"^              value: (.+)$", line)
    assert match, f"{name} must be a literal value"
    return match.group(1).strip('"')

assert env_value("DJINN_BUILD_ADMISSION_MODE") == expected_mode
assert env_value("DJINN_MAX_BUILD_TASKRUNS") == expected_cap
PY
}

expect_rejected() {
    local name=$1
    shift
    echo "=== invalid enforce: $name ==="
    if render "$TMPDIR_RENDER/$name.out" --set-string buildAdmission.mode=enforce "$@" 2>&1; then
        echo "FAIL: invalid Enforce scenario '$name' rendered successfully" >&2
        exit 1
    fi
}

echo "=== valid Enforce Recreate ==="
render "$TMPDIR_RENDER/enforce-recreate.yaml" \
    --set-string buildAdmission.mode=enforce \
    --set buildAdmission.maxBuildTaskRuns=4 \
    --set-string server.strategy.type=Recreate
assert_server_contract "$TMPDIR_RENDER/enforce-recreate.yaml" enforce 4 1 Recreate

echo "=== valid Enforce non-overlapping RollingUpdate ==="
render "$TMPDIR_RENDER/enforce-rolling.yaml" \
    --set-string buildAdmission.mode=enforce \
    --set buildAdmission.maxBuildTaskRuns=5 \
    --set-string server.strategy.type=RollingUpdate \
    --set server.strategy.rollingUpdate.maxSurge=0 \
    --set server.strategy.rollingUpdate.maxUnavailable=1
assert_server_contract "$TMPDIR_RENDER/enforce-rolling.yaml" enforce 5 1 RollingUpdate 0 1

expect_rejected zero-replicas --set server.replicas=0
expect_rejected multiple-replicas --set server.replicas=2
expect_rejected surge-capable --set server.strategy.rollingUpdate.maxSurge=1 --set server.strategy.rollingUpdate.maxUnavailable=0
expect_rejected unavailable-zero --set server.strategy.rollingUpdate.maxSurge=0 --set server.strategy.rollingUpdate.maxUnavailable=0
expect_rejected missing-mode --set-string buildAdmission.mode=
expect_rejected missing-cap --set-string buildAdmission.maxBuildTaskRuns=
expect_rejected zero-cap --set buildAdmission.maxBuildTaskRuns=0
expect_rejected oversized-cap --set buildAdmission.maxBuildTaskRuns=65

echo "=== Off bypasses the single-active topology ==="
render "$TMPDIR_RENDER/off.yaml" \
    --set-string buildAdmission.mode=off \
    --set buildAdmission.maxBuildTaskRuns=7 \
    --set server.replicas=2 \
    --set server.strategy.rollingUpdate.maxSurge=1 \
    --set server.strategy.rollingUpdate.maxUnavailable=0
assert_server_contract "$TMPDIR_RENDER/off.yaml" off 7 2 RollingUpdate 1 0

echo "=== Observe bypasses the single-active topology ==="
render "$TMPDIR_RENDER/observe.yaml" \
    --set-string buildAdmission.mode=observe \
    --set buildAdmission.maxBuildTaskRuns=8 \
    --set server.replicas=3 \
    --set server.strategy.rollingUpdate.maxSurge=2 \
    --set server.strategy.rollingUpdate.maxUnavailable=0
assert_server_contract "$TMPDIR_RENDER/observe.yaml" observe 8 3 RollingUpdate 2 0

echo "=== All build-admission topology Helm render tests passed ==="
