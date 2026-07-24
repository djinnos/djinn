#!/usr/bin/env bash
# Exact Helm render contract for the server allocator configuration.
# Usage: bash deploy/helm/djinn/tests/malloc-conf-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DEFAULT_VALUE="background_thread:true,dirty_decay_ms:10000,muzzy_decay_ms:10000"
OVERRIDE_VALUE="background_thread:false,dirty_decay_ms:5000,muzzy_decay_ms:0"
# Helm's --set-string parser uses commas to separate assignments, so preserve
# literal commas in the supplied value while asserting the unescaped render.
OVERRIDE_SET_VALUE="${OVERRIDE_VALUE//,/\\,}"

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
        --is-upgrade \
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

assert_server_backtrace() {
    local manifest=$1
    python3 - "$manifest" <<'PY'
import re
import sys

lines = open(sys.argv[1], encoding="utf-8").read().splitlines()
container_pattern = re.compile(r"^        - name: (.+)$")
locations = [i for i, line in enumerate(lines) if line == "            - name: RUST_BACKTRACE"]

assert len(locations) == 1, f"expected exactly one RUST_BACKTRACE entry, got {len(locations)}"
location = locations[0]
owner = next(
    (match.group(1) for line in reversed(lines[:location])
     if (match := container_pattern.match(line))),
    None,
)
assert owner == "djinn-server", f"RUST_BACKTRACE must be owned by djinn-server, got {owner!r}"
assert location + 1 < len(lines), "RUST_BACKTRACE is missing its value"
value_match = re.match(r'^              value: "?(.+?)"?$', lines[location + 1])
assert value_match and value_match.group(1) == "1", "RUST_BACKTRACE must have literal value 1"
PY
}

echo "=== default allocator configuration ==="
render "$TMPDIR_RENDER/default.yaml"
assert_malloc_conf "$TMPDIR_RENDER/default.yaml" "$DEFAULT_VALUE"
assert_server_backtrace "$TMPDIR_RENDER/default.yaml"

echo "=== overridden allocator configuration ==="
render "$TMPDIR_RENDER/override.yaml" --set-string "server.mallocConf=$OVERRIDE_SET_VALUE"
assert_malloc_conf "$TMPDIR_RENDER/override.yaml" "$OVERRIDE_VALUE"
assert_server_backtrace "$TMPDIR_RENDER/override.yaml"

echo "=== All MALLOC_CONF and RUST_BACKTRACE Helm render tests passed ==="
