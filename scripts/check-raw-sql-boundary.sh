#!/bin/sh
# Raw-SQL boundary guard.
#
# Direct sqlx query usage for Djinn's application database MUST live inside
# `server/crates/djinn-db/`. Other application crates should go through
# djinn-db's repository/query layer.
# This guard fails when changed Rust files outside approved boundaries
# introduce raw sqlx query calls (sqlx::query, sqlx::query!, etc.).
#
# Classification is DEFAULT-DENY. "Rust source" is not the same thing as
# "path ending in .rs": `.inc` files under server/ are `include!`d into a
# module and compiled verbatim, and nothing stops the next novel extension
# from appearing. So every changed path under the server source tree is
# sorted into exactly one of three buckets:
#
#   compiled      — inspected for raw sqlx usage (.rs, .inc).
#   inert         — data/config/docs that are never compiled into a crate;
#                   skipped silently (.sql, .toml, .json, .md, .snap, ...).
#   unclassified  — anything else. Inspected ANYWAY and announced with a
#                   ::warning::, so a novel extension is a visible skip
#                   rather than an invisible one. Silently dropping a
#                   compiled file is the failure mode this guard exists to
#                   prevent; add the extension to one of the two lists
#                   below to clear the warning.
#
# Usage:
#   BASE_SHA=<sha> ./scripts/check-raw-sql-boundary.sh
#   ./scripts/check-raw-sql-boundary.sh                      # uses locally available origin/main
#   printf '%s\n' server/crates/foo/src/lib.rs | ./scripts/check-raw-sql-boundary.sh --files-from-stdin
#   SQLX_GUARD_MODE=files-from-stdin ./scripts/check-raw-sql-boundary.sh < changed-files.txt
#
# Modes:
#   (default)       Diff BASE_SHA..HEAD to get changed Rust files, then check.
#   --files-from-stdin
#                   Read changed file paths from stdin; check only in-scope Rust files.
#
# Environment:
#   BASE_SHA        Base commit to diff HEAD against (default: locally available origin/main).
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
  BASE_SHA   Base commit to diff HEAD against (default: locally available origin/main).

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

# is_in_scope_path returns 0 if the path lives in the territory this guard
# polices, regardless of extension. Extension-based classification happens
# separately in classify_path so that an unrecognised extension inside the
# territory is loud instead of silently dropped.
is_in_scope_path() {
    path=$1

    # Must be under the server source tree.
    case "$path" in
        server/crates/*|server/src/*)
            ;;
        *)
            return 1
            ;;
    esac

    # Exclude the approved SQL boundary. djinn-db owns Djinn's application
    # database queries.
    case "$path" in
        server/crates/djinn-db/*)
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

# classify_path prints exactly one of: compiled | inert | unclassified.
#
# Callers must have already established that the path is in scope. Keeping
# the two lists adjacent makes it obvious that anything matching neither
# falls through to `unclassified` — which is inspected and reported, never
# skipped.
classify_path() {
    path=$1

    # Compiled into a crate. `.inc` files are pulled in with include!() and
    # are ordinary Rust source as far as rustc is concerned.
    case "$path" in
        *.rs|*.inc)
            echo compiled
            return 0
            ;;
    esac

    # Never compiled into a crate: fixtures, snapshots, schemas, docs,
    # scripts, and binary blobs that happen to live under server/.
    case "$path" in
        *.sql|*.toml|*.json|*.jsonl|*.md|*.mdx|*.yaml|*.yml|*.snap|*.txt \
        |*.lock|*.csv|*.tsv|*.html|*.css|*.ts|*.tsx|*.js|*.mjs|*.cjs \
        |*.py|*.sh|*.proto|*.graphql|*.patch|*.diff|*.env|*.png|*.jpg \
        |*.jpeg|*.gif|*.ico|*.svg|*.webp|*.gz|*.zip|*.tar|*.bin|*.wasm \
        |*.gitattributes|*.gitignore|*.dockerignore|*Dockerfile)
            echo inert
            return 0
            ;;
    esac

    echo unclassified
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
    unclassified=0

    while IFS= read -r file || [ -n "$file" ]; do
        [ -n "$file" ] || continue
        # Strip leading ./
        case "$file" in
            ./*) file=${file#./} ;;
        esac

        is_in_scope_path "$file" || continue
        # Skip deleted / nonexistent files.
        [ -f "$file" ] || continue

        kind=$(classify_path "$file")
        case "$kind" in
            inert)
                continue
                ;;
            unclassified)
                # Fail loud, not silent: we do not know whether this file is
                # compiled, so we inspect it and say so.
                unclassified=$((unclassified + 1))
                printf '::warning::check-raw-sql-boundary: unrecognised extension under the server source tree; inspecting it as if it were compiled Rust: %s\n' "$file" >&2
                printf '  Add this extension to the compiled or inert list in scripts/check-raw-sql-boundary.sh to silence this warning.\n' >&2
                ;;
        esac

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

    if [ "$unclassified" -gt 0 ]; then
        echo "OK: checked $checked compiled source file(s) outside djinn-db ($unclassified with an unrecognised extension, inspected anyway); no raw-sqlx boundary violations."
        exit 0
    fi

    echo "OK: checked $checked Rust source file(s) outside djinn-db; no raw-sqlx boundary violations."
    exit 0
}

# ── Diff mode: collect changed files from git ──────────────────────────

get_changed_files_from_diff() {
    echo "Checking raw-sqlx boundary against base $BASE_SHA ..."

    # Diff to find changed (Added, Modified, Copied, Renamed) files under the
    # server source tree. We only care about files that are present in the
    # working tree (so Deleted/Renamed-from are naturally skipped by subsequent
    # [ -f ] checks).
    #
    # The pathspec deliberately does NOT filter by extension. Extension
    # filtering lives in exactly one place — classify_path — because a second
    # copy of the rule here is a copy that can silently disagree with it. That
    # is precisely how `.inc` files went uninspected.
    git diff --name-only --diff-filter=ACMR "$BASE_SHA...HEAD" -- server/crates server/src || true
}

resolve_base_sha() {
    BASE_SHA=${BASE_SHA:-}
    if [ -z "$BASE_SHA" ]; then
        BASE_SHA=$(git rev-parse --verify origin/main^{commit} 2>/dev/null || true)
    else
        BASE_SHA=$(git rev-parse --verify "$BASE_SHA^{commit}" 2>/dev/null || true)
    fi

    if [ -z "$BASE_SHA" ]; then
        echo "::error::check-raw-sql-boundary: could not resolve a base commit. Set BASE_SHA to an available commit, or fetch origin/main outside this guard." >&2
        exit 2
    fi

    if ! git merge-base "$BASE_SHA" HEAD >/dev/null 2>&1; then
        echo "::error::check-raw-sql-boundary: $BASE_SHA has no locally available merge-base with HEAD. Set BASE_SHA to a commit with available history; this guard will not fetch or modify origin/main." >&2
        exit 2
    fi
}

# ── Main ───────────────────────────────────────────────────────────────

case "$MODE" in
    diff)
        resolve_base_sha
        get_changed_files_from_diff | check_files
        ;;
    files-from-stdin)
        check_files
        ;;
esac
