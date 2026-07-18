#!/usr/bin/env bash
# Regression contract for the projects-PVC environment passed to djinn-server.
# Usage: bash deploy/helm/djinn/tests/projects-pvc-configmap-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

command -v helm >/dev/null 2>&1 || {
    echo "FAIL: required test tool 'helm' is not installed" >&2
    exit 1
}

TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

assert_projects_pvc() {
    local fullname=$1 expected=$2 output="$TMPDIR_RENDER/$fullname.yaml"
    helm template projects-pvc-test "$CHART_DIR" \
        --show-only templates/configmap.yaml \
        --set-string "fullnameOverride=$fullname" > "$output"

    local line count
    line="  DJINN_K8S_PROJECTS_PVC: \"$expected\""
    count=$(grep -Fxc "$line" "$output" || true)
    if [[ "$count" != 1 ]]; then
        echo "FAIL: expected exactly one ConfigMap entry '$line', got $count" >&2
        cat "$output" >&2
        exit 1
    fi
}

assert_projects_pvc "owner-cache" "owner-cache-projects"
assert_projects_pvc "another-owner-cache" "another-owner-cache-projects"
echo "=== DJINN_K8S_PROJECTS_PVC ConfigMap render test passed ==="
