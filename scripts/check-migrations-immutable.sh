#!/bin/sh
# Migration-immutability guard.
#
# Already-merged sqlx migrations are IMMUTABLE. sqlx records a checksum of each
# migration when it is applied and refuses to start if an applied migration's
# file has changed ("migration N was previously applied but has been modified").
# Editing, renaming, or deleting a migration that already exists on the base
# branch therefore breaks the `migrate` init container on EVERY existing
# database (staging, prod, the VPS) even though a fresh DB still works — so it
# sails through tests and only detonates at deploy time.
#
# This guard fails the build if any migration file that exists on the base ref
# is Modified, Renamed, or Deleted by the change set. Adding NEW migration files
# is the only permitted change. Schema changes must always be a new migration.
#
# Usage:
#   BASE_SHA=<sha> ./scripts/check-migrations-immutable.sh
#   ./scripts/check-migrations-immutable.sh            # falls back to origin/main
#
# Environment:
#   BASE_SHA   Base commit to diff HEAD against. If unset, origin/main is used.
#   MIGRATIONS_GLOB
#              Pathspec for migration files
#              (default: server/crates/djinn-db/migrations_postgres).
#
# There is intentionally NO opt-out marker. If you genuinely must change an
# applied migration (you almost never should), revert the file and add a new
# migration that performs the corrective DDL instead.

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

MIGRATIONS_GLOB=${MIGRATIONS_GLOB:-server/crates/djinn-db/migrations_postgres}

BASE_SHA=${BASE_SHA:-}
if [ -z "$BASE_SHA" ]; then
    git fetch --no-tags --depth=1 origin main >/dev/null 2>&1 || true
    BASE_SHA=$(git rev-parse --verify origin/main 2>/dev/null || true)
fi

if [ -z "$BASE_SHA" ]; then
    echo "::error::check-migrations-immutable: could not determine a base SHA to diff against." >&2
    exit 1
fi

echo "Checking migration immutability against base $BASE_SHA ..."

# --diff-filter=MRD = Modified, Renamed, Deleted. Added (A) is intentionally
# excluded — new migration files are the only permitted change. The three-dot
# form diffs against the merge base of BASE_SHA and HEAD, tolerating a slightly
# stale base SHA after main advances.
#
# --name-status output for renames is "Rxxx\told\tnew"; -z keeps it parseable
# but for a human-facing guard the plain names are sufficient. We list the
# offending paths and fail.
offenders=$(git diff --name-only --diff-filter=MRD "$BASE_SHA...HEAD" -- "$MIGRATIONS_GLOB" || true)

if [ -n "$offenders" ]; then
    echo "::error::Existing migration files were modified, renamed, or deleted. Applied migrations are immutable — add a NEW migration instead. Offending files:" >&2
    printf '%s\n' "$offenders" | while IFS= read -r f; do
        [ -n "$f" ] && echo "  - $f" >&2
    done
    echo "" >&2
    echo "Why this is blocked: sqlx checksums each migration when applied and refuses to boot if an applied migration changed, breaking deploys on every existing database (the new schema still works on a fresh DB, so it passes tests). Revert these files and express the schema change as a new migration." >&2
    exit 1
fi

echo "OK: no existing migrations were modified, renamed, or deleted."
