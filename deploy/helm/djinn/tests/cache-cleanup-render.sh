#!/usr/bin/env bash
# Exact Helm render contract for the coordinator cache-cleanup mode.
# Usage: bash deploy/helm/djinn/tests/cache-cleanup-render.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURE_DIR="$SCRIPT_DIR/fixtures/cache-cleanup"
require_tool() { command -v "$1" >/dev/null 2>&1 || { echo "FAIL: required test tool '$1' is not installed" >&2; exit 1; }; }
assert_fixture() {
    local actual=$1 fixture=$2
    [[ -f "$fixture" ]] || { echo "FAIL: missing expected fixture: $fixture" >&2; exit 1; }
    cmp -s "$actual" "$fixture" || { echo "FAIL: rendered cache-cleanup contract differs from $fixture" >&2; diff -u "$fixture" "$actual" >&2 || true; exit 1; }
}
require_tool helm
require_tool python3
TMPDIR_RENDER=$(mktemp -d)
FIXTURE_BASELINE="$TMPDIR_RENDER/fixtures.sha256"
sha256sum "$FIXTURE_DIR"/*.env > "$FIXTURE_BASELINE"
trap 'rm -rf "$TMPDIR_RENDER"' EXIT
render_contract() {
    local output=$1
    shift
    helm template cache-cleanup-test "$CHART_DIR" "$@" > "$TMPDIR_RENDER/manifest.yaml"
    python3 - "$TMPDIR_RENDER/manifest.yaml" > "$output" <<'PY'
import re
import sys
lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
entries = []
for i, line in enumerate(lines):
    if line == "            - name: DJINN_CACHE_CLEANUP_MODE":
        assert i + 1 < len(lines) and lines[i + 1].startswith("              value: "), "mode must be literal"
        entries.append((i, lines[i + 1].split(": ", 1)[1].strip('"')))
assert len(entries) == 1, f"expected exactly one mode env entry, got {len(entries)}"
i, mode = entries[0]
containers = max(n for n in range(i + 1) if lines[n] == "      containers:")
container = next(lines[n].split(": ", 1)[1] for n in range(containers + 1, i + 1) if re.match(r"^        - name: ", lines[n]))
assert container == "djinn-server", f"mode must be on djinn-server, got {container}"
print(f"DJINN_CACHE_CLEANUP_MODE={mode}")
PY
}
for scenario in fresh-default upgrade-default local operator-dry-run server-env; do
    echo "=== $scenario ==="
    case "$scenario" in
        fresh-default|server-env) render_contract "$TMPDIR_RENDER/$scenario.env" ;;
        upgrade-default) render_contract "$TMPDIR_RENDER/$scenario.env" --is-upgrade ;;
        local) render_contract "$TMPDIR_RENDER/$scenario.env" --values "$CHART_DIR/values.local.yaml" ;;
        operator-dry-run) render_contract "$TMPDIR_RENDER/$scenario.env" --set-string cacheCleanup.mode=dry_run ;;
    esac
    assert_fixture "$TMPDIR_RENDER/$scenario.env" "$FIXTURE_DIR/$scenario.env"
done
echo "=== invalid value ==="
if helm template cache-cleanup-test "$CHART_DIR" --set-string cacheCleanup.mode=invalid > "$TMPDIR_RENDER/invalid.out" 2>&1; then
    echo "FAIL: invalid cacheCleanup.mode rendered successfully" >&2
    exit 1
fi
printf 'invalid cacheCleanup.mode rejected\n' > "$TMPDIR_RENDER/invalid.env"
assert_fixture "$TMPDIR_RENDER/invalid.env" "$FIXTURE_DIR/invalid.env"
sha256sum --check --status "$FIXTURE_BASELINE" || {
    echo "FAIL: render harness mutated an expected fixture" >&2
    exit 1
}
echo "=== All cache-cleanup Helm render tests passed ==="
