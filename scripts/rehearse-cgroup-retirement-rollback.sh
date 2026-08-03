#!/bin/sh
# Run the repository-only aggregate RETIRE rollback rehearsal. This fixture
# creates its own temporary Git repository and never changes this worktree.
set -eu
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
NODE_BIN=$(command -v node 2>/dev/null || true)
if [ -z "$NODE_BIN" ] && [ -x /opt/node/bin/node ]; then NODE_BIN=/opt/node/bin/node; fi
if [ -z "$NODE_BIN" ]; then printf 'FATAL: node is required for rollback rehearsal\n' >&2; exit 2; fi
exec "$NODE_BIN" "$SCRIPT_DIR/rehearse-cgroup-retirement-rollback.mjs" "$@"
