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
    if [ -n "${LINKED_WORKTREE:-}" ] && [ -d "$LINKED_WORKTREE" ]; then
        git -C "$REPO_ROOT" worktree remove --force "$LINKED_WORKTREE" 2>/dev/null || rm -rf -- "$LINKED_WORKTREE"
    fi
    rm -rf -- "$REPO_ROOT/$FIXTURE_BASE" 2>/dev/null || true
    if [ -n "${LOG_DIR:-}" ] && [ -d "$LOG_DIR" ]; then
        rm -rf -- "$LOG_DIR"
    fi
}

# Run the guard from a linked worktree, which shares remote-tracking refs with
# the caller's common Git directory. This catches regressions where a guard
# fetch mutates origin/main for every worktree.
run_linked_worktree_diff_guard() {
    LINKED_WORKTREE="$LOG_DIR/linked-worktree"
    git -C "$REPO_ROOT" worktree add --detach "$LINKED_WORKTREE" HEAD > "$LOG_DIR/t14_worktree_setup.log" 2>&1
    cp "$GUARD" "$LINKED_WORKTREE/scripts/check-raw-sql-boundary.sh"

    ORIGIN_MAIN_BEFORE=$(git -C "$REPO_ROOT" rev-parse --verify origin/main^{commit})
    SHALLOW_BEFORE=$(git -C "$REPO_ROOT" rev-parse --is-shallow-repository)
    LINKED_GIT_DIR=$(git -C "$LINKED_WORKTREE" rev-parse --git-dir)
    LINKED_COMMON_DIR=$(git -C "$LINKED_WORKTREE" rev-parse --git-common-dir)

    set +e
    (cd "$LINKED_WORKTREE" && unset BASE_SHA && sh scripts/check-raw-sql-boundary.sh) > "$LOG_DIR/t14_linked_worktree.log.out" 2>&1
    T14_ACTUAL=$?
    set -e

    ORIGIN_MAIN_AFTER=$(git -C "$REPO_ROOT" rev-parse --verify origin/main^{commit})
    SHALLOW_AFTER=$(git -C "$REPO_ROOT" rev-parse --is-shallow-repository)
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

# ── T20: .inc files are compiled source and MUST be inspected ─────────
#
# Regression test for the hole this suite did not have: `.inc` files are
# include!()d into a .rs module and compiled verbatim, but the guard's
# candidate filter only accepted `*.rs`, so a real violation in
# graph_tools/tests_coverage.inc sat in the tree with a green gate.
FIXTURE_INC="$REPO_ROOT/$FIXTURE_BASE/src/tests_seed.inc"
cat > "$FIXTURE_INC" <<'FIXTURE'
    async fn seed(db: &Database) {
        sqlx::query("INSERT INTO projects (id, name) VALUES ($1,$2)")
            .bind("p1")
            .bind("p1")
            .execute(db.pool())
            .await
            .expect("seed project");
    }
FIXTURE

INC_PATH="$FIXTURE_BASE/src/tests_seed.inc"
set +e
run_guard t20_inc_violation "$INC_PATH"
t20_actual=$?
set -e
assert_exit "T20 sqlx::query in a .inc file exits non-zero" 1 "$t20_actual" "$LOG_DIR/t20_inc_violation.log.out"
assert_output_contains "T20 reports the violating .inc file" \
    "::error::Raw sqlx query usage detected outside djinn-db: $INC_PATH" \
    "$LOG_DIR/t20_inc_violation.log.out"
assert_output_lacks "T20 does not warn about .inc as unrecognised" \
    "unrecognised extension" "$LOG_DIR/t20_inc_violation.log.out"

# ── T21: a clean .inc file passes and is counted as checked ───────────
FIXTURE_INC_CLEAN="$REPO_ROOT/$FIXTURE_BASE/src/tests_clean.inc"
cat > "$FIXTURE_INC_CLEAN" <<'FIXTURE'
    async fn seed(db: &Database) {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id("p1", "p1", "test", "p1")
            .await
            .expect("seed project");
    }
FIXTURE

INC_CLEAN_PATH="$FIXTURE_BASE/src/tests_clean.inc"
set +e
run_guard t21_inc_clean "$INC_CLEAN_PATH"
t21_actual=$?
set -e
assert_exit "T21 clean .inc file exits 0" 0 "$t21_actual" "$LOG_DIR/t21_inc_clean.log.out"
assert_output_contains "T21 counts the .inc file as checked" \
    "checked 1 Rust source file(s)" "$LOG_DIR/t21_inc_clean.log.out"

# ── T22: an unrecognised extension is LOUD, not silently skipped ──────
#
# The defect was never `.inc` specifically — it was that a file the guard
# declines to classify vanishes without a trace. An unknown extension under
# the server source tree must be announced AND inspected, so a novel
# compiled extension can never pass quietly.
FIXTURE_UNKNOWN="$REPO_ROOT/$FIXTURE_BASE/src/generated_queries.rs2"
cat > "$FIXTURE_UNKNOWN" <<'FIXTURE'
pub async fn fetch(pool: &sqlx::PgPool) {
    sqlx::query("SELECT 1").execute(pool).await.unwrap();
}
FIXTURE

UNKNOWN_PATH="$FIXTURE_BASE/src/generated_queries.rs2"
set +e
run_guard t22_unknown_ext "$UNKNOWN_PATH"
t22_actual=$?
set -e
assert_exit "T22 unrecognised extension with a violation exits non-zero" 1 "$t22_actual" "$LOG_DIR/t22_unknown_ext.log.out"
assert_output_contains "T22 warns about the unrecognised extension" \
    "::warning::check-raw-sql-boundary: unrecognised extension under the server source tree; inspecting it as if it were compiled Rust: $UNKNOWN_PATH" \
    "$LOG_DIR/t22_unknown_ext.log.out"
assert_output_contains "T22 still reports the violation" \
    "::error::Raw sqlx query usage detected outside djinn-db: $UNKNOWN_PATH" \
    "$LOG_DIR/t22_unknown_ext.log.out"

# ── T23: a clean unrecognised extension warns but does not fail ───────
FIXTURE_UNKNOWN_CLEAN="$REPO_ROOT/$FIXTURE_BASE/src/notes.rs2"
cat > "$FIXTURE_UNKNOWN_CLEAN" <<'FIXTURE'
pub const GREETING: &str = "hello";
FIXTURE

UNKNOWN_CLEAN_PATH="$FIXTURE_BASE/src/notes.rs2"
set +e
run_guard t23_unknown_clean "$UNKNOWN_CLEAN_PATH"
t23_actual=$?
set -e
assert_exit "T23 clean unrecognised extension exits 0" 0 "$t23_actual" "$LOG_DIR/t23_unknown_clean.log.out"
assert_output_contains "T23 still warns about the unrecognised extension" \
    "unrecognised extension" "$LOG_DIR/t23_unknown_clean.log.out"
assert_output_contains "T23 summary names the unclassified count" \
    "1 with an unrecognised extension, inspected anyway" \
    "$LOG_DIR/t23_unknown_clean.log.out"

# ── T24: inert data files under server/ are skipped silently ──────────
#
# The loud-unclassified rule must not turn every fixture into noise. A .sql
# fixture living under server/crates is data, not compiled source.
FIXTURE_SQL="$REPO_ROOT/$FIXTURE_BASE/fixtures/seed.sql"
mkdir -p -- "$(dirname -- "$FIXTURE_SQL")"
cat > "$FIXTURE_SQL" <<'FIXTURE'
INSERT INTO projects (id, name) VALUES ('p1', 'p1');
FIXTURE

SQL_FIXTURE_PATH="$FIXTURE_BASE/fixtures/seed.sql"
set +e
run_guard t24_inert "$SQL_FIXTURE_PATH"
t24_actual=$?
set -e
assert_exit "T24 inert .sql fixture exits 0" 0 "$t24_actual" "$LOG_DIR/t24_inert.log.out"
assert_output_lacks "T24 does not warn about inert data files" \
    "unrecognised extension" "$LOG_DIR/t24_inert.log.out"

# ── T25: sqlx syntax inside a string literal is DATA, not a call ──────
#
# djinn-graph/src/db_access.rs is a scanner that detects SQL in source text,
# and its unit tests feed it Rust source as a string. The guard was matching
# its own test fixtures. Rust never compiles the contents of a string
# literal, so a literal cannot contain a call.
#
# The fixture below reproduces both real shapes from that file AND, on the
# lines immediately adjacent, a genuine violation — because the rule that
# fixes the false positive must not be able to swallow the real thing.
FIXTURE_SCANNER="$REPO_ROOT/$FIXTURE_BASE/src/scanner_selftest.rs"
cat > "$FIXTURE_SCANNER" <<'FIXTURE'
#[cfg(test)]
mod tests {
    #[test]
    fn detects_insert_into() {
        let hits = scan_sql("sqlx::query!(\"INSERT INTO orders (sku) VALUES (?)\", sku);");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn ignores_select_without_from() {
        let hits = scan_sql("let v = sqlx::query_scalar!(\"SELECT 1\");");
        assert!(hits.is_empty());
    }
}
FIXTURE

SCANNER_PATH="$FIXTURE_BASE/src/scanner_selftest.rs"
set +e
run_guard t25_scanner_literals "$SCANNER_PATH"
t25_actual=$?
set -e
assert_exit "T25 sqlx syntax inside a string literal exits 0" 0 "$t25_actual" "$LOG_DIR/t25_scanner_literals.log.out"
assert_output_lacks "T25 does not flag the scanner self-test fixtures" \
    "::error::Raw sqlx query usage detected outside djinn-db: $SCANNER_PATH" \
    "$LOG_DIR/t25_scanner_literals.log.out"

# ── T26: a real violation ADJACENT to such a literal is still caught ───
#
# This is the load-bearing test for T25. If the string-literal rule were
# implemented as "skip lines that contain quotes" or "skip this file", the
# real call two lines down would vanish with the fixtures.
FIXTURE_ADJACENT="$REPO_ROOT/$FIXTURE_BASE/src/scanner_adjacent.rs"
cat > "$FIXTURE_ADJACENT" <<'FIXTURE'
#[cfg(test)]
mod tests {
    #[test]
    fn detects_insert_into() {
        let hits = scan_sql("sqlx::query!(\"INSERT INTO orders (sku) VALUES (?)\", sku);");
        sqlx::query("DELETE FROM orders").execute(pool).await.unwrap();
        let more = scan_sql("let v = sqlx::query_scalar!(\"SELECT 1\");");
        assert_eq!(hits.len(), more.len());
    }
}
FIXTURE

ADJACENT_PATH="$FIXTURE_BASE/src/scanner_adjacent.rs"
set +e
run_guard t26_adjacent_violation "$ADJACENT_PATH"
t26_actual=$?
set -e
assert_exit "T26 real violation adjacent to string literals exits non-zero" 1 "$t26_actual" "$LOG_DIR/t26_adjacent_violation.log.out"
assert_output_contains "T26 reports the adjacent violation" \
    "::error::Raw sqlx query usage detected outside djinn-db: $ADJACENT_PATH" \
    "$LOG_DIR/t26_adjacent_violation.log.out"
assert_output_contains "T26 names the real call's line, not the fixtures'" \
    "6:        sqlx::query(\"DELETE FROM orders\")" \
    "$LOG_DIR/t26_adjacent_violation.log.out"
assert_output_lacks "T26 does not report the line 5 fixture" \
    "5:        let hits = scan_sql" "$LOG_DIR/t26_adjacent_violation.log.out"
assert_output_lacks "T26 does not report the line 7 fixture" \
    "7:        let more = scan_sql" "$LOG_DIR/t26_adjacent_violation.log.out"

# ── T27: SQL built by concatenation is still a violation ───────────────
#
# The narrowness requirement: only the matched TOKENS being literal data is
# excused, never the contents of a literal. `credential.rs` builds SQL with
# format! on the same line as the call — a "quotes on this line" rule would
# have blinded the guard to exactly that shape.
FIXTURE_CONCAT="$REPO_ROOT/$FIXTURE_BASE/src/concat_sql.rs"
cat > "$FIXTURE_CONCAT" <<'FIXTURE'
const SET: &str = "UPDATE credentials SET value = $1 WHERE ";

pub async fn upsert(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(&format!("{SET}owner_user_id = $3"))
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}
FIXTURE

CONCAT_PATH="$FIXTURE_BASE/src/concat_sql.rs"
set +e
run_guard t27_concat "$CONCAT_PATH"
t27_actual=$?
set -e
assert_exit "T27 format!-built SQL still exits non-zero" 1 "$t27_actual" "$LOG_DIR/t27_concat.log.out"
assert_output_contains "T27 reports the concatenation violation" \
    "::error::Raw sqlx query usage detected outside djinn-db: $CONCAT_PATH" \
    "$LOG_DIR/t27_concat.log.out"

# ── T28: a call AFTER a closed literal on the same line still fails ────
#
# Escape handling matters: `"a\"b"` is ONE literal. Naive quote counting
# would close it at the escaped quote, treat the rest of the line as string
# body, and silently swallow the call that follows.
FIXTURE_AFTER="$REPO_ROOT/$FIXTURE_BASE/src/after_literal.rs"
cat > "$FIXTURE_AFTER" <<'FIXTURE'
pub async fn go(pool: &sqlx::PgPool) {
    let label = "a\"b"; sqlx::query("SELECT 1").execute(pool).await.unwrap();
    let ch = '"'; sqlx::query_scalar("SELECT 2").fetch_one(pool).await.unwrap();
    let _ = (label, ch);
}
FIXTURE

AFTER_PATH="$FIXTURE_BASE/src/after_literal.rs"
set +e
run_guard t28_after_literal "$AFTER_PATH"
t28_actual=$?
set -e
assert_exit "T28 call after a closed literal exits non-zero" 1 "$t28_actual" "$LOG_DIR/t28_after_literal.log.out"
assert_output_contains "T28 catches the call after an escaped-quote literal" \
    "2:    let label = " "$LOG_DIR/t28_after_literal.log.out"
assert_output_contains "T28 catches the call after a double-quote char literal" \
    "3:    let ch = " "$LOG_DIR/t28_after_literal.log.out"

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

# ── T13: default diff mode does not mutate shared origin/main ──────────
run_linked_worktree_diff_guard
assert_exit "T13 linked-worktree default diff exits 0" 0 "$T14_ACTUAL" "$LOG_DIR/t14_linked_worktree.log.out"
if [ "$LINKED_GIT_DIR" != "$LINKED_COMMON_DIR" ]; then
    pass "T13 executes from a linked worktree with a shared Git directory"
else
    fail "T13 executes from a linked worktree with a shared Git directory" "git-dir and git-common-dir were both $LINKED_GIT_DIR"
fi
if [ "$ORIGIN_MAIN_BEFORE" = "$ORIGIN_MAIN_AFTER" ]; then
    pass "T13 preserves shared origin/main"
else
    fail "T13 preserves shared origin/main" "before=$ORIGIN_MAIN_BEFORE after=$ORIGIN_MAIN_AFTER"
fi
if [ "$SHALLOW_BEFORE" = "$SHALLOW_AFTER" ]; then
    pass "T13 preserves shared repository shallow state"
else
    fail "T13 preserves shared repository shallow state" "before=$SHALLOW_BEFORE after=$SHALLOW_AFTER"
fi

# ── summary ────────────────────────────────────────────────────────────
printf -- '------------------------------------------\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
exit 0
