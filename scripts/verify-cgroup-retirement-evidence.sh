#!/bin/sh
# Deterministically validate repository-captured cgroup-retirement evidence.
# This checker intentionally has no Kubernetes, Docker, or credential inputs.
# Usage: ./scripts/verify-cgroup-retirement-evidence.sh --candidate RETIRE_HEAD
#        ./scripts/verify-cgroup-retirement-evidence.sh --landing <40-hex-commit>
# Exit: 0 valid, 1 evidence rejected, 2 invocation/repository failure.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=${CGROUP_RETIREMENT_ROOT:-"$SCRIPT_DIR/fixtures/cgroup-retirement"}

if [ "$#" -ne 2 ] || { [ "$1" != "--candidate" ] && [ "$1" != "--landing" ]; }; then
    printf 'usage: %s --candidate RETIRE_HEAD | --landing <40-hex-commit>\n' "$0" >&2
    exit 2
fi
SUBJECT=$2
case "$SUBJECT" in
    ''|*[!A-Za-z0-9_.-]*) printf 'REJECT evidence: invalid identity\n' >&2; exit 1 ;;
esac
if [ "$1" = "--landing" ]; then
    case "$SUBJECT" in *[!0123456789abcdef]*) printf 'REJECT landing: commit must be lowercase 40-hex identity\n' >&2; exit 1 ;; esac
    if [ "${#SUBJECT}" -ne 40 ]; then
        printf 'REJECT landing: commit must be lowercase 40-hex identity\n' >&2
        exit 1
    fi
fi

NODE_BIN=$(command -v node 2>/dev/null || true)
if [ -z "$NODE_BIN" ] && [ -x /opt/node/bin/node ]; then NODE_BIN=/opt/node/bin/node; fi
if [ -z "$NODE_BIN" ]; then
    printf 'FATAL: node is required to validate integer evidence\n' >&2
    exit 2
fi
if [ "$1" = "--landing" ]; then
    [ -r "$ROOT/landing/$SUBJECT.json" ] && [ -r "$ROOT/landing/$SUBJECT.outcome.json" ] || { printf 'REJECT landing: missing deterministic landing fixture %s\n' "$SUBJECT" >&2; exit 1; }
    exec "$NODE_BIN" "$SCRIPT_DIR/verify-cgroup-retirement-landing.mjs" "$ROOT" "$SUBJECT"
fi
if [ ! -r "$ROOT/schema.json" ] || [ ! -r "$ROOT/PREP_HEAD.json" ] || [ ! -r "$ROOT/candidates/$SUBJECT.json" ]; then
    printf 'REJECT evidence: missing immutable schema, PREP_HEAD, or candidate %s\n' "$SUBJECT" >&2
    exit 1
fi
exec "$NODE_BIN" "$SCRIPT_DIR/verify-cgroup-retirement-evidence.mjs" "$ROOT" "$SUBJECT"
