#!/bin/sh
# Deterministically validate repository-captured cgroup-retirement evidence.
# This checker intentionally has no Kubernetes, Docker, or credential inputs.
# Usage: ./scripts/verify-cgroup-retirement-evidence.sh --candidate RETIRE_HEAD
# Exit: 0 valid, 1 evidence rejected, 2 invocation/repository failure.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=${CGROUP_RETIREMENT_ROOT:-"$SCRIPT_DIR/fixtures/cgroup-retirement"}

if [ "$#" -ne 2 ] || [ "$1" != "--candidate" ]; then
    printf 'usage: %s --candidate RETIRE_HEAD\n' "$0" >&2
    exit 2
fi
CANDIDATE=$2
case "$CANDIDATE" in
    ''|*[!A-Za-z0-9_.-]*) printf 'REJECT candidate: invalid candidate identity\n' >&2; exit 1 ;;
esac

NODE_BIN=$(command -v node 2>/dev/null || true)
if [ -z "$NODE_BIN" ] && [ -x /opt/node/bin/node ]; then NODE_BIN=/opt/node/bin/node; fi
if [ -z "$NODE_BIN" ]; then
    printf 'FATAL: node is required to validate integer evidence\n' >&2
    exit 2
fi
if [ ! -r "$ROOT/schema.json" ] || [ ! -r "$ROOT/PREP_HEAD.json" ] || [ ! -r "$ROOT/candidates/$CANDIDATE.json" ]; then
    printf 'REJECT evidence: missing immutable schema, PREP_HEAD, or candidate %s\n' "$CANDIDATE" >&2
    exit 1
fi
exec "$NODE_BIN" "$SCRIPT_DIR/verify-cgroup-retirement-evidence.mjs" "$ROOT" "$CANDIDATE"
