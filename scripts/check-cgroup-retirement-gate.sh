#!/bin/sh
# Fail-closed repository gate for cgroup-launcher retirement PREP ranges and
# candidate actions. It never contacts a cluster and cannot authorize rollout.
#
# Usage:
#   ./scripts/check-cgroup-retirement-gate.sh --prep PREP_BASE PREP_HEAD
#   ./scripts/check-cgroup-retirement-gate.sh --deploy --candidate RETIRE_HEAD --inputs path.json
# Actions --release and --withdraw-node use the same mandatory-proof interlock.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DEFAULT_REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
ROOT=${CGROUP_RETIREMENT_GATE_ROOT:-$DEFAULT_REPO_ROOT}
ENGINE="$SCRIPT_DIR/cgroup-retirement-gate.mjs"

if [ ! -d "$ROOT" ] || [ ! -s "$ENGINE" ] || [ ! -x "$SCRIPT_DIR/verify-cgroup-retirement-evidence.sh" ]; then
    printf 'FATAL: cgroup-retirement gate prerequisites are missing\n' >&2
    exit 2
fi
NODE_BIN=$(command -v node 2>/dev/null || true)
if [ -z "$NODE_BIN" ] && [ -x /opt/node/bin/node ]; then NODE_BIN=/opt/node/bin/node; fi
if [ -z "$NODE_BIN" ]; then
    printf 'FATAL: node is required for cgroup-retirement gate\n' >&2
    exit 2
fi
exec "$NODE_BIN" "$ENGINE" "$@"
