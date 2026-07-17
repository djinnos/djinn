#!/bin/sh
# Post-cutover guard for the durable Phase 1 knowledge deletion ledger.
# It never mutates DB records or performs Phase 2 live-state migration.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GENERATOR="$SCRIPT_DIR/djinn-retirement-manifest.mjs"
FIXTURE_DIR="$SCRIPT_DIR/fixtures/djinn-retirement"
LEDGER="$FIXTURE_DIR/deletion-ledger.json"
DB_GUIDANCE="$FIXTURE_DIR/db-guidance.json"

for f in "$GENERATOR" "$LEDGER" "$DB_GUIDANCE"; do
  if [ ! -s "$f" ]; then
    printf 'FATAL: required retirement guard input not found: %s\n' "$f" >&2
    exit 2
  fi
done

NODE_BIN=$(command -v node 2>/dev/null || true)
if [ -z "$NODE_BIN" ] && [ -x /opt/node/bin/node ]; then
  NODE_BIN=/opt/node/bin/node
fi
if [ -z "$NODE_BIN" ]; then
  printf 'FATAL: node is required but was not found on PATH or /opt/node/bin\n' >&2
  exit 2
fi

printf 'Validating durable retirement ledger and post-cutover tracked state...\n'
cd "$REPO_ROOT"
git ls-files -z '.djinn/*' | "$NODE_BIN" "$GENERATOR" \
  --deletion-ledger "$LEDGER" \
  --db-guidance "$DB_GUIDANCE"
printf 'OK: durable deletion evidence is valid and tracked project-local knowledge is empty.\n'
