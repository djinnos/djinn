#!/usr/bin/env bash
# Exact Helm render contract for the server allocator configuration.
# Usage: bash deploy/helm/djinn/tests/malloc-conf-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DEFAULT_VALUE="background_thread:true,dirty_decay_ms:10000,muzzy_decay_ms:10000"
OVERRIDE_VALUE="background_thread:false,dirty_decay_ms:5000,muzzy_decay_ms:0"

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
    helm template malloc-conf-test "$CHART_DIR" \
        --show-only templates/deployment-server.yaml "$@" > "$output"
}

assert_malloc_conf() {
    local manifest=$1 expected=$2
    python3 - "$manifest" "$expected" <<'PY'
import re
import sys

manifest, expected = sys.argv[1:]
lines = open(manifest, encoding="utf-8").read().splitlines()
container_pattern = re.compile(r"^        - name: (.+)$")
env_marker = "            - name: MALLOC_CONF"
locations = [i for i, line in enumerate(lines) if line == env_marker]

assert len(locations) == 2, (
    f"expected exactly two MALLOC_CONF entries (one per djinn-server invocation), got {len(locations)}"
)

owners = []
for location in locations:
    owner = next(
        (match.group(1) for line in reversed(lines[:location])
         if (match := container_pattern.match(line))),
        None,
    )
    assert owner is not None, "MALLOC_CONF is not owned by a container"
    assert location + 1 < len(lines), "MALLOC_CONF is missing its value"
    value_match = re.match(r"^              value: (.+)$", lines[location + 1])
    assert value_match, "MALLOC_CONF must be a literal value"
    actual = value_match.group(1).strip('"')
    assert actual == expected, f"MALLOC_CONF for {owner} was {actual!r}, expected {expected!r}"
    owners.append(owner)

assert sorted(owners) == ["djinn-server", "migrate"], (
    f"MALLOC_CONF must appear once on migrate and once on djinn-server, got {owners}"
)
PY
}

echo "=== default allocator configuration ==="
render "$TMPDIR_RENDER/default.yaml"
assert_malloc_conf "$TMPDIR_RENDER/default.yaml" "$DEFAULT_VALUE"

echo "=== overridden allocator configuration ==="
render "$TMPDIR_RENDER/override.yaml" --set-string "server.mallocConf=$OVERRIDE_VALUE"
assert_malloc_conf "$TMPDIR_RENDER/override.yaml" "$OVERRIDE_VALUE"

echo "=== All MALLOC_CONF Helm render tests passed ==="
