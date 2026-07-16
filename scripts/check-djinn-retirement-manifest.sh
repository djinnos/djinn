#!/bin/sh
# Strict reconciliation guard for the Phase 1 knowledge retirement manifest
# generator (epic h1w2 / task mbfw).
#
# Runs the hermetic manifest generator against the live repository HEAD with
# the committed DB-selection/guidance fixtures and validates that:
#
#   1. The generator derives the complete tracked `.djinn` knowledge set from
#      `git ls-files -z` (no hard-coded baseline count).
#   2. Both manifests are emitted under target/djinn-retirement/ with
#      deterministic ordering and all required fields.
#   3. Every entry has exactly one disposition and the strict invariants hold.
#
# This guard does NOT mutate production DB records, delete tracked `.djinn/`
# files, or perform Phase 2 live-state migration. Generated output under
# target/ is transient and gitignored.
#
# Usage:
#   ./scripts/check-djinn-retirement-manifest.sh
#
# Exits 0 on success, 1 on any invariant violation.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

GENERATOR="$SCRIPT_DIR/djinn-retirement-manifest.mjs"
FIXTURE_DIR="$SCRIPT_DIR/fixtures/djinn-retirement"
DB_SELECTION="$FIXTURE_DIR/db-selection.json"
DB_GUIDANCE="$FIXTURE_DIR/db-guidance.json"
OUTPUT_DIR="$REPO_ROOT/target/djinn-retirement"

if [ ! -f "$GENERATOR" ]; then
  printf 'FATAL: generator not found at %s\n' "$GENERATOR" >&2
  exit 2
fi
if [ ! -f "$DB_SELECTION" ]; then
  printf 'FATAL: DB selection fixture not found at %s\n' "$DB_SELECTION" >&2
  exit 2
fi
if [ ! -f "$DB_GUIDANCE" ]; then
  printf 'FATAL: DB guidance fixture not found at %s\n' "$DB_GUIDANCE" >&2
  exit 2
fi

# Locate node: prefer PATH, fall back to the bundled /opt/node/bin used by the
# Rust+Node task image. CI runners have node on PATH already.
NODE_BIN=$(command -v node 2>/dev/null || true)
if [ -z "$NODE_BIN" ] && [ -x /opt/node/bin/node ]; then
  NODE_BIN=/opt/node/bin/node
fi
if [ -z "$NODE_BIN" ]; then
  printf 'FATAL: node is required but was not found on PATH or /opt/node/bin\n' >&2
  exit 2
fi

printf 'Running hermetic retirement manifest generator against HEAD...\n'
cd "$REPO_ROOT"

# Feed NUL-delimited git ls-files output directly to the generator.
# The generator reads committed blob bytes via `git show HEAD:<path>` and
# writes both manifests under OUTPUT_DIR.
git ls-files -z '.djinn/*' | "$NODE_BIN" "$GENERATOR" \
  --db-selection "$DB_SELECTION" \
  --db-guidance "$DB_GUIDANCE" \
  --output-dir "$OUTPUT_DIR"

# Verify the manifests exist and are non-empty.
KNOWLEDGE_MANIFEST="$OUTPUT_DIR/knowledge-manifest.json"
GUIDANCE_MANIFEST="$OUTPUT_DIR/db-guidance-manifest.json"
for f in "$KNOWLEDGE_MANIFEST" "$GUIDANCE_MANIFEST"; do
  if [ ! -s "$f" ]; then
    printf 'FAIL: manifest not written or empty: %s\n' "$f" >&2
    exit 1
  fi
done

# The generator's generateAll() already runs full strict validation internally:
# count/set mismatch, ambiguous permalinks, duplicate paths, missing preserved
# identity, empty discard reason / approving task id, missing guidance
# disposition, and unresolved entries all cause a non-zero exit above.
printf 'OK: retirement manifests are valid and consistent with the tracked knowledge set.\n'
