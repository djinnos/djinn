#!/bin/sh
# Raw-SQL boundary guard.
#
# Direct sqlx query usage for Djinn's application database MUST live inside
# `server/crates/djinn-db/`. Other application crates should go through
# djinn-db's repository/query layer. The catalog wrapper is also excluded: it
# administers isolated databases in an external catalog service and therefore
# cannot route those service-control statements through Djinn's repository.
# This guard fails when changed Rust files outside approved boundaries
# introduce raw sqlx query calls (sqlx::query, sqlx::query!, etc.).
#
# Usage:
#   BASE_SHA=<sha> ./scripts/check-raw-sql-boundary.sh
#   ./scripts/check-raw-sql-boundary.sh                      # falls back to origin/main
#   printf '%s\n' server/crates/foo/src/lib.rs | ./scripts/check-raw-sql-boundary.sh --files-from-stdin
#   SQLX_GUARD_MODE=files-from-stdin ./scripts/check-raw-sql-boundary.sh < changed-files.txt
#
# Modes:
#   (default)       Diff BASE_SHA..HEAD to get changed Rust files, then check.
#   --files-from-stdin
#                   Read changed file paths from stdin; check only in-scope Rust files.
#
# Environment:
#   BASE_SHA        Base commit to diff HEAD against (default: origin/main).
#   SQLX_GUARD_MODE Override mode selection: "diff" or "files-from-stdin".
#
# Exit codes:
#   0  No violations found (or no in-scope files to check).
#   1  Violations found — raw sqlx query usage detected outside djinn-db.
#   2  Usage error.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

MODE=${SQLX_GUARD_MODE:-diff}

usage() {
    cat <<EOF
Usage: $0 [--diff|--files-from-stdin]

Modes:
  --diff              Diff BASE_SHA..HEAD for changed Rust files (default; also SQLX_GUARD_MODE=diff).
  --files-from-stdin  Read changed file paths from stdin (also SQLX_GUARD_MODE=files-from-stdin).

Environment:
  BASE_SHA   Base commit to diff HEAD against (default: origin/main).

Checks changed Rust files outside the approved SQL boundaries for raw sqlx
query usage (sqlx::query, sqlx::query!, sqlx::query_as!, sqlx::query_scalar!,
and same-module imported forms like query!, query_as!, query_scalar!,
query_scalar).
EOF
}

if [ "$#" -gt 1 ]; then
    usage >&2
    exit 2
fi

if [ "$#" -eq 1 ]; then
    case "$1" in
        --diff)
            MODE=diff
            ;;
        --files-from-stdin)
            MODE=files-from-stdin
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
fi

case "$MODE" in
    diff|files-from-stdin)
        ;;
    *)
        printf 'Unknown SQLX_GUARD_MODE: %s\n' "$MODE" >&2
        usage >&2
        exit 2
        ;;
esac

# ── Scope / filter helpers ─────────────────────────────────────────────

# is_in_scope_rs_file returns 0 if the path is a Rust source file we should
# inspect. Generated paths, the djinn-db crate, and the external-service catalog
# wrapper are excluded.
is_in_scope_rs_file() {
    path=$1

    # Must be a .rs file.
    case "$path" in
        *.rs)
            ;;
        *)
            return 1
            ;;
    esac

    # Must be under the server source tree.
    case "$path" in
        server/crates/*.rs|server/src/*.rs)
            ;;
        *)
            return 1
            ;;
    esac

    # Exclude the approved SQL boundaries. djinn-db owns Djinn's application
    # database queries. djinn-catalog-wrapper intentionally issues DDL against
    # separately cataloged tenant services; those statements do not belong in
    # Djinn's application repository layer.
    case "$path" in
        server/crates/djinn-db/*|server/crates/djinn-catalog-wrapper/*)
            return 1
            ;;
    esac

    # Exclude generated files defensively.
    case "$path" in
        */generated/*|*.gen.*)
            return 1
            ;;
    esac

    return 0
}

# ── Grep patterns ──────────────────────────────────────────────────────
#
# We match the following forms:
#   Fully-qualified:
#     sqlx::query(
#     sqlx::query!(
#     sqlx::query_as(
#     sqlx::query_as!(
#     sqlx::query_scalar(
#     sqlx::query_scalar!
#
#   Same-module imports (after `use sqlx::...`):
#     query(
#     query!(
#     query_as(
#     query_as!(
#     query_scalar(
#     query_scalar!
#
#   Also catches `use sqlx::{query, ...}` import lines themselves as an
#   indicator that the module intends to use raw SQL.

# The grep pattern for fully-qualified sqlx calls and same-module usage.
# We use egrep (POSIX) with alternation. Word-boundary is approximated by
# requiring a non-alphanumeric character before the match start where needed.
SQL_PATTERN='(sqlx::query[!(]|sqlx::query_as[!(]|sqlx::query_scalar[!(]|(^|[^a-zA-Z_])query[!(]|(^|[^a-zA-Z_])query_as[!(]|(^|[^a-zA-Z_])query_scalar[!(]|use sqlx::.*query)'

check_files() {
    violations=0
    checked=0

    while IFS= read -r file || [ -n "$file" ]; do
        [ -n "$file" ] || continue
        # Strip leading ./
        case "$file" in
            ./*) file=${file#./} ;;
        esac

        is_in_scope_rs_file "$file" || continue
        # Skip deleted / nonexistent files.
        [ -f "$file" ] || continue

        checked=$((checked + 1))

        # Grep the file for sqlx query patterns.
        # -E = extended regex (POSIX), -n = line numbers.
        hits=$(grep -E -n "$SQL_PATTERN" "$file" 2>/dev/null || true)
        if [ -n "$hits" ]; then
            violations=$((violations + 1))
            printf '::error::Raw sqlx query usage detected outside djinn-db: %s\n' "$file" >&2
            printf '%s\n' "$hits" | while IFS= read -r line; do
                [ -n "$line" ] && printf '  %s\n' "$line" >&2
            done
            echo >&2
        fi
    done

    if [ "$checked" -eq 0 ]; then
        echo "Checked 0 Rust source file(s) outside djinn-db; no raw-sqlx boundary violations."
        exit 0
    fi

    if [ "$violations" -gt 0 ]; then
        printf '\n' >&2
        echo "::error::Found $violations file(s) with raw sqlx query usage outside server/crates/djinn-db/. All direct sqlx query calls must go through the djinn-db repository layer." >&2
        exit 1
    fi

    echo "OK: checked $checked Rust source file(s) outside djinn-db; no raw-sqlx boundary violations."
    exit 0
}

# ── Diff mode: collect changed files from git ──────────────────────────

get_changed_files_from_diff() {
    BASE_SHA=${BASE_SHA:-}
    if [ -z "$BASE_SHA" ]; then
        git fetch --no-tags --depth=1 origin main >/dev/null 2>&1 || true
        BASE_SHA=$(git rev-parse --verify origin/main 2>/dev/null || true)
    fi

    if [ -z "$BASE_SHA" ]; then
        echo "::error::check-raw-sql-boundary: could not determine a base SHA to diff against." >&2
        exit 1
    fi

    echo "Checking raw-sqlx boundary against base $BASE_SHA ..."

    # Diff to find changed (Added, Modified, Copied) Rust files. We only care
    # about files that are present in the working tree (so Deleted/Renamed-from
    # are naturally skipped by subsequent [ -f ] checks).
    git diff --name-only --diff-filter=ACMR "$BASE_SHA...HEAD" -- '*.rs' || true
}

# ── Main ───────────────────────────────────────────────────────────────

case "$MODE" in
    diff)
        get_changed_files_from_diff | check_files
        ;;
    files-from-stdin)
        check_files
        ;;
esac
