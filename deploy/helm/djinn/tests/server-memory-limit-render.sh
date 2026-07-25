#!/usr/bin/env bash
# Helm contract for the chart-to-server memory-limit byte environment variable.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
command -v helm >/dev/null 2>&1 || { echo "FAIL: helm is required" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "FAIL: python3 is required" >&2; exit 1; }
TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT
render() { local output=$1; shift; helm template server-memory-limit-test "$CHART_DIR" --is-upgrade --show-only templates/deployment-server.yaml "$@" >"$output"; }
assert_limit_ref() {
python3 - "$1" <<'PY'
import sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
marker = "            - name: DJINN_SERVER_MEMORY_LIMIT_BYTES"
locations = [i for i, line in enumerate(lines) if line == marker]
assert len(locations) == 1, f"expected one memory-limit env entry, got {len(locations)}"
assert lines[locations[0]:locations[0]+6] == [marker, "              valueFrom:", "                resourceFieldRef:", "                  containerName: djinn-server", "                  resource: limits.memory", '                  divisor: "1"']
PY
}
render "$TMPDIR_RENDER/default.yaml"; assert_limit_ref "$TMPDIR_RENDER/default.yaml"
render "$TMPDIR_RENDER/override.yaml" --set-string resources.server.limits.memory=1536Mi; assert_limit_ref "$TMPDIR_RENDER/override.yaml"
echo "=== server memory-limit Helm render contract passed ==="
