#!/bin/sh
# The no-retirement gate for proposal 3i92's in-place task-run resize cutover
# (epic xowm, task 1j64).
#
# The cutover moves ONE container's CPU ceiling. It retires nothing. This gate
# refuses any diff that deletes or DISABLES the launcher crate, the broker, the
# writable-cgroup RuntimeClass and node assets, the credential-boundary chart
# tests, the launcher child mount set, the `cgroup.kill` write, `wait_empty`, or
# the resize-v2-only condition on the launcher CPU ceiling.
#
# It is NOT a byte-identity check and must never become one — see the header of
# scripts/resize-cutover-retention-manifest.mjs for why that formulation is
# unsatisfiable in this epic.
#
# The exit code IS the contract:
#   0  every protected asset is retained
#   1  a retirement was detected
#   2  the gate could not run at all
#
# Usage:
#   ./scripts/check-resize-cutover-retention.sh
#   RESIZE_RETENTION_ROOT=/path/to/fixture ./scripts/check-resize-cutover-retention.sh
#     (self-test hook; see scripts/test-resize-cutover-retention.mjs)

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DEFAULT_REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
ROOT=${RESIZE_RETENTION_ROOT:-$DEFAULT_REPO_ROOT}
ENGINE="$SCRIPT_DIR/resize-cutover-retention-manifest.mjs"

if [ ! -s "$ENGINE" ]; then
    printf 'FATAL: the retention engine is missing: %s\n' "$ENGINE" >&2
    exit 2
fi

if [ ! -d "$ROOT" ]; then
    printf 'FATAL: retention root is not a directory: %s\n' "$ROOT" >&2
    exit 2
fi

NODE_BIN=$(command -v node 2>/dev/null || true)
if [ -z "$NODE_BIN" ] && [ -x /opt/node/bin/node ]; then
    NODE_BIN=/opt/node/bin/node
fi
if [ -z "$NODE_BIN" ]; then
    printf 'FATAL: node is required but was not found on PATH or /opt/node/bin\n' >&2
    exit 2
fi

# `exec` deliberately: the engine's exit code is this script's exit code, with
# nothing in between that could turn a 1 into a 0.
exec "$NODE_BIN" "$ENGINE" --root "$ROOT"
