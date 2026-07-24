#!/bin/sh
# Self-test harness for scripts/check-raw-sql-boundary.sh.
#
# Exercises the production guard end-to-end against synthetic fixture files
# that this script creates (and tears down) under the repository's
# server/crates/ tree. Pure POSIX shell; no cargo, no python, no network.
#
# Run from the repository root:
#
#   sh scripts/test-check-raw-sql-boundary.sh
#
# Exits 0 on success. The first failing assertion aborts the harness with a
# non-zero status, and the EXIT trap still removes every fixture path.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GUARD="$SCRIPT_DIR/check-raw-sql-boundary.sh"
FIXTURE_BASE="server/crates/djinn_sqlx_guard_fixture"

cleanup() {
    rm -rf -- "$REPO_ROOT/$FIXTURE_BASE" 2>/dev/null || true
    if [ -n "${LOG_DIR:-}" ] && [ -d "$LOG_DIR" ]; then
        rm -rf -- "$LOG_DIR"
    fi
}
trap cleanup EXIT INT TERM

if [ ! -f "$GUARD" ]; then
    printf 'FATAL: production guard not found at %s\n' "$GUARD" >&2
    exit 2
fi

PASS=0
FAIL=0
# Prefer /var/tmp (disk-backed, always writable in CI sandboxes).
LOG_DIR=$(mktemp -d /var/tmp/djinn-sqlx-guard-test.XXXXXX 2>/dev/null || \
          mktemp -d "$HOME/.cache/djinn/djinn-sqlx-guard-test.XXXXXX" 2>/dev/null || \
          mktemp -d "${TMPDIR:-.}/djinn-sqlx-guard-test.XXXXXX")
if [ ! -d "$LOG_DIR" ]; then
    printf 'FATAL: could not create scratch log dir\n' >&2
    exit 2
fi

pass() {
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
}

fail() {
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n' "$1" >&2
    if [ -n "${2:-}" ]; then
        printf '       %s\n' "$2" >&2
    fi
}

# run_guard <label> [paths...]
#
# Pipes the supplied paths (one per line) into the production guard via
# --files-from-stdin mode. When paths is empty, the guard sees an empty stdin.
# Returns the guard's exit status.
run_guard() {
    label=$1
    shift

    log="$LOG_DIR/$label.log"
    out="$LOG_DIR/$label.log.out"

    if [ "$#" -eq 0 ]; then
        : > "$log"
    else
        printf '%s\n' "$@" > "$log"
    fi

    cd "$REPO_ROOT" && env \
        SQLX_GUARD_MODE=files-from-stdin \
        sh "$GUARD" --files-from-stdin < "$log" > "$out" 2>&1
    return $?
}

assert_exit() {
    label=$1
    expected=$2
    actual=$3
    log_path=$4

    if [ "$expected" -eq 0 ] && [ "$actual" -eq 0 ]; then
        pass "$label"
    elif [ "$expected" -ne 0 ] && [ "$actual" -ne 0 ]; then
        pass "$label (exit=$actual)"
    else
        fail "$label" "expected exit=$expected, got exit=$actual
output:
$(cat "$log_path")"
    fi
}

assert_output_contains() {
    label=$1
    needle=$2
    log_path=$3

    if grep -q -- "$needle" "$log_path"; then
        pass "$label"
    else
        fail "$label" "expected output to contain '$needle'
actual output:
$(cat "$log_path")"
    fi
}

assert_output_lacks() {
    label=$1
    needle=$2
    log_path=$3

    if grep -q -- "$needle" "$log_path"; then
        fail "$label" "expected output to NOT contain '$needle'
actual output:
$(cat "$log_path")"
    else
        pass "$label"
    fi
}

# Always start from a clean slate.
rm -rf -- "$REPO_ROOT/$FIXTURE_BASE"

printf '== running self-tests for scripts/check-raw-sql-boundary.sh ==\n'

# ── T1: empty stdin exits 0 ───────────────────────────────────────────
set +e
run_guard t1_empty
t1_actual=$?
set -e
assert_exit "T1 empty stdin exits 0" 0 "$t1_actual" "$LOG_DIR/t1_empty.log.out"
assert_output_contains "T1 reports no violations" \
    "no raw-sqlx boundary violations" "$LOG_DIR/t1_empty.log.out"

# ── T2: non-Rust files are ignored ────────────────────────────────────
set +e
run_guard t2_non_rust \
    "scripts/check-raw-sql-boundary.sh" \
    "docs/architecture.md" \
    "ui/src/api/client.ts"
t2_actual=$?
set -e
assert_exit "T2 non-Rust files exit 0" 0 "$t2_actual" "$LOG_DIR/t2_non_rust.log.out"

# ── T3: nonexistent files are skipped ─────────────────────────────────
set +e
run_guard t3_nonexistent \
    "server/crates/fake-crate/src/does_not_exist.rs"
t3_actual=$?
set -e
assert_exit "T3 nonexistent file exits 0" 0 "$t3_actual" "$LOG_DIR/t3_nonexistent.log.out"

# ── T4: violation — sqlx::query outside djinn-db ──────────────────────
FIXTURE_VIOLATION="$REPO_ROOT/$FIXTURE_BASE/src/lib.rs"
mkdir -p -- "$(dirname -- "$FIXTURE_VIOLATION")"
cat > "$FIXTURE_VIOLATION" <<'FIXTURE'
use sqlx::PgPool;

pub async fn bad_query(pool: &PgPool) -> i64 {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .unwrap();
    count
}
FIXTURE

VIOLATION_PATH="$FIXTURE_BASE/src/lib.rs"
set +e
run_guard t4_violation "$VIOLATION_PATH"
t4_actual=$?
set -e
assert_exit "T4 sqlx::query_scalar violation exits non-zero" 1 "$t4_actual" "$LOG_DIR/t4_violation.log.out"
assert_output_contains "T4 reports the violating file" \
    "::error::Raw sqlx query usage detected outside djinn-db: $VIOLATION_PATH" \
    "$LOG_DIR/t4_violation.log.out"
assert_output_contains "T4 mentions sqlx boundary in summary" \
    "raw sqlx query usage outside server/crates/djinn-db" \
    "$LOG_DIR/t4_violation.log.out"

# ── T5: same-module query! macro import — violation ───────────────────
FIXTURE_MACRO="$REPO_ROOT/$FIXTURE_BASE/src/macro_use.rs"
cat > "$FIXTURE_MACRO" <<'FIXTURE'
use sqlx::{query, query_scalar};

pub async fn fetch(pool: &sqlx::PgPool) -> String {
    let name: String = query_scalar!("SELECT name FROM items WHERE id = $1", 1i64)
        .fetch_one(pool)
        .await
        .unwrap();
    name
}
FIXTURE

MACRO_PATH="$FIXTURE_BASE/src/macro_use.rs"
set +e
run_guard t5_macro_import "$MACRO_PATH"
t5_actual=$?
set -e
assert_exit "T5 use sqlx::query macro violation exits non-zero" 1 "$t5_actual" "$LOG_DIR/t5_macro_import.log.out"
assert_output_contains "T5 reports the violating file" \
    "::error::Raw sqlx query usage detected outside djinn-db: $MACRO_PATH" \
    "$LOG_DIR/t5_macro_import.log.out"

# ── T6: file under djinn-db is exempted ───────────────────────────────
# We pass a path under server/crates/djinn-db/ — the guard should skip it
# even though the file does not exist (the guard filters by path prefix,
# not file content, for djinn-db exemption).
DJINNDB_PATH="server/crates/djinn-db/src/repos/example.rs"
set +e
run_guard t6_djinn_db_exemption "$DJINNDB_PATH"
t6_actual=$?
set -e
assert_exit "T6 djinn-db path exits 0" 0 "$t6_actual" "$LOG_DIR/t6_djinn_db_exemption.log.out"
assert_output_contains "T6 reports no violations" \
    "no raw-sqlx boundary violations" "$LOG_DIR/t6_djinn_db_exemption.log.out"

# ── T7: clean file outside djinn-db passes ────────────────────────────
FIXTURE_CLEAN="$REPO_ROOT/$FIXTURE_BASE/src/clean.rs"
mkdir -p -- "$(dirname -- "$FIXTURE_CLEAN")"
cat > "$FIXTURE_CLEAN" <<'FIXTURE'
pub struct UserRepo;

impl UserRepo {
    pub async fn find(&self, _id: &str) -> String {
        String::new()
    }
}
FIXTURE

CLEAN_PATH="$FIXTURE_BASE/src/clean.rs"
set +e
run_guard t7_clean "$CLEAN_PATH"
t7_actual=$?
set -e
assert_exit "T7 clean file exits 0" 0 "$t7_actual" "$LOG_DIR/t7_clean.log.out"
assert_output_contains "T7 reports OK" \
    "no raw-sqlx boundary violations" "$LOG_DIR/t7_clean.log.out"

# ── T8: sqlx::query_as! violation ─────────────────────────────────────
FIXTURE_QUERY_AS="$REPO_ROOT/$FIXTURE_BASE/src/query_as.rs"
cat > "$FIXTURE_QUERY_AS" <<'FIXTURE'
pub async fn get_user(pool: &sqlx::PgPool) -> (String, String) {
    sqlx::query_as!(r#"SELECT name, email FROM users"#)
        .fetch_one(pool)
        .await
        .unwrap()
}
FIXTURE

QUERY_AS_PATH="$FIXTURE_BASE/src/query_as.rs"
set +e
run_guard t8_query_as "$QUERY_AS_PATH"
t8_actual=$?
set -e
assert_exit "T8 sqlx::query_as! violation exits non-zero" 1 "$t8_actual" "$LOG_DIR/t8_query_as.log.out"
assert_output_contains "T8 reports query_as! violation" \
    "::error::Raw sqlx query usage detected outside djinn-db: $QUERY_AS_PATH" \
    "$LOG_DIR/t8_query_as.log.out"

# ── T9: sqlx::query! macro violation ──────────────────────────────────
FIXTURE_QUERY_MACRO="$REPO_ROOT/$FIXTURE_BASE/src/query_macro.rs"
cat > "$FIXTURE_QUERY_MACRO" <<'FIXTURE'
pub async fn insert_order(pool: &sqlx::PgPool, sku: &str) {
    sqlx::query!("INSERT INTO orders (sku) VALUES ($1)", sku)
        .execute(pool)
        .await
        .unwrap();
}
FIXTURE

QUERY_MACRO_PATH="$FIXTURE_BASE/src/query_macro.rs"
set +e
run_guard t9_query_macro "$QUERY_MACRO_PATH"
t9_actual=$?
set -e
assert_exit "T9 sqlx::query! macro violation exits non-zero" 1 "$t9_actual" "$LOG_DIR/t9_query_macro.log.out"
assert_output_contains "T9 reports query! macro violation" \
    "::error::Raw sqlx query usage detected outside djinn-db: $QUERY_MACRO_PATH" \
    "$LOG_DIR/t9_query_macro.log.out"

# ── T10: mixed files — violation + clean + djinn-db ───────────────────
set +e
run_guard t10_mixed "$VIOLATION_PATH" "$CLEAN_PATH" "$DJINNDB_PATH"
t10_actual=$?
set -e
assert_exit "T10 mixed files exits non-zero (violation present)" 1 "$t10_actual" "$LOG_DIR/t10_mixed.log.out"
assert_output_contains "T10 reports violation file" \
    "::error::Raw sqlx query usage detected outside djinn-db: $VIOLATION_PATH" \
    "$LOG_DIR/t10_mixed.log.out"
assert_output_lacks "T10 does not flag djinn-db path" \
    "::error::Raw sqlx query usage detected outside djinn-db: $DJINNDB_PATH" \
    "$LOG_DIR/t10_mixed.log.out"
assert_output_lacks "T10 does not flag clean file" \
    "::error::Raw sqlx query usage detected outside djinn-db: $CLEAN_PATH" \
    "$LOG_DIR/t10_mixed.log.out"

# ── T11: --help exits 0 ───────────────────────────────────────────────
set +e
cd "$REPO_ROOT" && sh "$GUARD" --help > "$LOG_DIR/t11_help.log.out" 2>&1
t11_actual=$?
set -e
assert_exit "T11 --help exits 0" 0 "$t11_actual" "$LOG_DIR/t11_help.log.out"
assert_output_contains "T11 help mentions files-from-stdin" \
    "files-from-stdin" "$LOG_DIR/t11_help.log.out"

# ── T12: sqlx::query (runtime, non-macro) violation ───────────────────
FIXTURE_RUNTIME="$REPO_ROOT/$FIXTURE_BASE/src/runtime_query.rs"
cat > "$FIXTURE_RUNTIME" <<'FIXTURE'
pub async fn update_status(pool: &sqlx::PgPool, id: &str) {
    sqlx::query("UPDATE items SET status = 'done' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}
FIXTURE

RUNTIME_PATH="$FIXTURE_BASE/src/runtime_query.rs"
set +e
run_guard t12_runtime_query "$RUNTIME_PATH"
t12_actual=$?
set -e
assert_exit "T12 sqlx::query() runtime violation exits non-zero" 1 "$t12_actual" "$LOG_DIR/t12_runtime_query.log.out"
assert_output_contains "T12 reports runtime query violation" \
    "::error::Raw sqlx query usage detected outside djinn-db: $RUNTIME_PATH" \
    "$LOG_DIR/t12_runtime_query.log.out"

# ── T13: catalog service wrapper is an approved SQL boundary ──────────
# Unlike application crates, this wrapper administers an external Postgres
# service and cannot route tenant CREATE/DROP statements through djinn-db.
CATALOG_WRAPPER_PATH="server/crates/djinn-catalog-wrapper/src/lib.rs"
set +e
run_guard t13_catalog_wrapper_exemption "$CATALOG_WRAPPER_PATH"
t13_actual=$?
set -e
assert_exit "T13 catalog wrapper path exits 0" 0 "$t13_actual" "$LOG_DIR/t13_catalog_wrapper_exemption.log.out"
assert_output_contains "T13 reports no violations" \
    "no raw-sqlx boundary violations" "$LOG_DIR/t13_catalog_wrapper_exemption.log.out"

# ── summary ────────────────────────────────────────────────────────────
printf -- '------------------------------------------\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
exit 0
