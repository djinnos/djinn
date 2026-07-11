#!/bin/sh
# Pre-merge deterministic gate runner.
#
# Runs the *deterministic, locally-runnable* server-scope checks that gate the
# merge queue in `.github/workflows/quality-gate.yml` but only ever execute on
# the `merge_group` base ref (or as required Quality-Gate sub-jobs), so a PR can
# look green, get approved, and then be rejected in the merge queue with an
# identical failure fingerprint. Running these before a worker requests review
# (or before opening a PR) turns those post-approval merge-queue rejections into
# a fast local fix.
#
# Every gate here delegates to the SAME repo script / command CI runs — there is
# no reimplemented logic that can drift from the workflow. The Rust pre-approval
# gate (`server/crates/djinn-coordinator/src/preapproval_gate.rs`,
# `SERVER_CHECK_SET`) mirrors this exact set; a drift-guard test keeps the two in
# lockstep.
#
# Scope: the FAST, no-compile deterministic guards run by default (size guard,
# migrations-immutability guard, raw-SQL boundary, capability boundaries,
# architectural boundaries). These need no Cargo build and no database, so they
# finish in seconds. The compile/DB-heavy gates (sqlx offline-cache freshness)
# are opt-in via `--with-sqlx` / `--all`; the full test suite is intentionally
# NOT run here (that is the merge queue's job).
#
# Usage:
#   ./scripts/premerge-gates.sh                 # fast deterministic guards
#   ./scripts/premerge-gates.sh --with-sqlx     # also `make sqlx-check`
#   ./scripts/premerge-gates.sh --all           # every deterministic gate
#   BASE_SHA=<sha> ./scripts/premerge-gates.sh  # pin the diff base explicitly
#
# Environment:
#   BASE_SHA   Base commit the changed-file gates diff HEAD against. When unset,
#              resolved as the merge-base against origin/main (then main), with a
#              HEAD fallback — mirroring the workflow's BASE_SHA logic.
#
# Exit codes:
#   0  All selected gates passed.
#   1  One or more gates failed (all gates still run — fail-fast is off, matching
#      the workflow's `fail-fast: false`).
#   2  Usage error.

set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

WITH_SQLX=0
for arg in "$@"; do
    case "$arg" in
        --with-sqlx)
            WITH_SQLX=1
            ;;
        --all)
            WITH_SQLX=1
            ;;
        -h|--help)
            sed -n '2,45p' "$0"
            exit 0
            ;;
        *)
            printf 'Unknown argument: %s\n' "$arg" >&2
            printf 'Usage: %s [--with-sqlx|--all]\n' "$0" >&2
            exit 2
            ;;
    esac
done

# ── Resolve the diff base once and share it with every changed-file gate ──────
# Mirrors the workflow: prefer the merge-base against origin/main, then main;
# fall back to HEAD (an empty diff) so the size guard still runs cleanly on a
# detached/base-less checkout. The boundary/migration guards require a real base
# to diff against, so if none resolves we surface that as a hard usage error
# rather than silently passing.
if [ -z "${BASE_SHA:-}" ]; then
    BASE_SHA=$(git merge-base origin/main HEAD 2>/dev/null \
        || git merge-base main HEAD 2>/dev/null \
        || git rev-parse HEAD 2>/dev/null \
        || true)
fi
export BASE_SHA

if [ -z "$BASE_SHA" ]; then
    echo "::error::premerge-gates: could not resolve a diff base (no origin/main, main, or HEAD)." >&2
    exit 2
fi

echo "premerge-gates: diffing changed files against base $BASE_SHA"
echo

# ── Gate runner ───────────────────────────────────────────────────────────────
# Each gate prints its own diagnostics; we record pass/fail and keep going so a
# single run surfaces every failing gate at once.
FAILED=""

run_gate() {
    name=$1
    shift
    echo "═══ $name ═══"
    if "$@"; then
        echo "PASS  $name"
    else
        echo "FAIL  $name"
        FAILED="$FAILED $name"
    fi
    echo
}

# Size guard — changed-file mode (same as the server-size-guard job). Deleted
# files are excluded (--diff-filter=AMR); only in-scope, still-present Rust
# files are measured.
size_guard() {
    git diff --name-only --diff-filter=AMR "$BASE_SHA...HEAD" \
        | ./scripts/check-file-size.sh --files-from-stdin
}

# Migration-immutability guard — applied migrations are immutable; only NEW
# migration files are permitted. Reads BASE_SHA from the environment.
migrations_guard() {
    ./scripts/check-migrations-immutable.sh
}

# Raw-SQL boundary — direct sqlx query usage must stay inside djinn-db.
raw_sql_boundary() {
    ./scripts/check-raw-sql-boundary.sh
}

# Capability boundaries — self-test the detectors first (fails loudly on a
# detector/allowlist regression), then run the live git/http/k8s detectors.
capability_boundaries() {
    ./scripts/test-capability-boundaries.sh \
        && ./scripts/check-git-boundary.sh \
        && ./scripts/check-http-boundary.sh \
        && ./scripts/check-k8s-boundary.sh
}

# Architectural boundaries — forbidden-edge crate/file layering rules. Reads
# manifests and greps source only (no DB, no compile).
architectural_boundaries() {
    python3 scripts/check_boundaries.py
}

# sqlx offline-cache freshness — opt-in (needs a reachable Postgres + a Cargo
# check compile). `make sqlx-check` brings the docker test Postgres up to schema
# then runs the DB-agnostic verifier, matching the server-sqlx-cache job.
sqlx_offline_cache() {
    make sqlx-check
}

run_gate "size_guard"              size_guard
run_gate "migrations_guard"        migrations_guard
run_gate "raw_sql_boundary"        raw_sql_boundary
run_gate "capability_boundaries"   capability_boundaries
run_gate "architectural_boundaries" architectural_boundaries

if [ "$WITH_SQLX" = "1" ]; then
    run_gate "sqlx_offline_cache"  sqlx_offline_cache
fi

# ── Summary ───────────────────────────────────────────────────────────────────
if [ -n "$FAILED" ]; then
    echo "premerge-gates: FAILED gates:$FAILED"
    echo "These would fail the PR Quality Gate / merge queue. Fix them before requesting review."
    exit 1
fi

echo "premerge-gates: all selected gates passed."
exit 0
