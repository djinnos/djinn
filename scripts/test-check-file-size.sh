#!/bin/sh
# Self-test harness for scripts/check-file-size.sh.
#
# Exercises the production reporter end-to-end against synthetic fixture files
# that this script creates (and tears down) under the repository's
# server/crates/ tree. Pure POSIX shell; no cargo, no python, no network.
#
# The reporter is report-only: the central property asserted here is that an
# oversized file with NO `djinn:allow-oversize` marker is still *named* in the
# ranked report and still exits 0. It used to exit 1; see the rationale block
# at the top of check-file-size.sh for why that stopped.
#
# Run from the repository root:
#
#   sh scripts/test-check-file-size.sh
#
# Exits 0 on success. The first failing assertion aborts the harness with a
# non-zero status, and the EXIT trap still removes every fixture path.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GUARD="$SCRIPT_DIR/check-file-size.sh"
FIXTURE_BASE="server/crates/djinn_size_guard_fixture"

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
# Prefer /var/tmp (disk-backed, always writable in CI sandboxes), then the
# per-run djinn cache, then $TMPDIR. Avoid $TMPDIR=/tmp on hosts that
# sandbox tmp — the fallback chain keeps this test usable everywhere.
LOG_DIR=$(mktemp -d /var/tmp/djinn-size-guard-test.XXXXXX 2>/dev/null || \
          mktemp -d "$HOME/.cache/djinn/djinn-size-guard-test.XXXXXX" 2>/dev/null || \
          mktemp -d "${TMPDIR:-.}/djinn-size-guard-test.XXXXXX")
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

# run_guard <label> <mode-arg> [paths...]
#
# Pipes the supplied paths (one per line) into the production guard.
# When paths is empty, the guard sees an empty stdin — which exercises
# the "no changed files" case the CI relies on for PRs that don't touch
# any in-scope Rust file. Captures the guard's stdout/stderr in
# $LOG_DIR/$label.log.out and returns the guard's exit status.
run_guard() {
    label=$1
    mode=$2
    shift 2

    log="$LOG_DIR/$label.log"
    out="$LOG_DIR/$label.log.out"

    if [ "$#" -eq 0 ]; then
        : > "$log"
    else
        # Word-split the path list on purpose: each argument is one path.
        # shellcheck disable=SC2086
        printf '%s\n' "$@" > "$log"
    fi

    cd "$REPO_ROOT" && env \
        MAX_LINES=2 \
        MAX_BYTES=64 \
        SIZE_GUARD_MODE="$mode" \
        GITHUB_STEP_SUMMARY="$LOG_DIR/$label.summary.md" \
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

# Always start from a clean slate in case a prior aborted run left a fixture
# directory behind.
rm -rf -- "$REPO_ROOT/$FIXTURE_BASE"

printf '== running self-tests for scripts/check-file-size.sh ==\n'

# ── T1: changed-file mode with empty input ────────────────────────────
# Mirrors the CI path where a PR diff has no in-scope Rust files.
set +e
run_guard t1_empty files-from-stdin
t1_actual=$?
set -e
assert_exit "T1 empty stdin exits 0" 0 "$t1_actual" "$LOG_DIR/t1_empty.log.out"
assert_output_contains "T1 reports zero files checked" \
    "Checked 0 Rust source file(s)" "$LOG_DIR/t1_empty.log.out"

# ── T2: out-of-scope paths are filtered out ───────────────────────────
# Files under server/scripts/, scripts/, ui/, and the production guard
# itself are not in scope. They must be ignored, not flagged.
set +e
run_guard t2_out_of_scope files-from-stdin \
    scripts/check-file-size.sh \
    server/scripts/release.sh \
    ui/src/api/generated/mcp-tools.gen.ts \
    docs/some-note.md
t2_actual=$?
set -e
assert_exit "T2 out-of-scope paths exit 0" 0 "$t2_actual" "$LOG_DIR/t2_out_of_scope.log.out"
assert_output_contains "T2 reports zero files checked" \
    "Checked 0 Rust source file(s)" "$LOG_DIR/t2_out_of_scope.log.out"

# ── T3: synthetic oversized .rs under server/crates/ ─────────────────
# Create a fixture that is intentionally larger than the low thresholds
# (MAX_LINES=2, MAX_BYTES=64) used by run_guard. The fixture lives under
# server/crates/ so it is in scope for the guard.
FIXTURE_OVER="$REPO_ROOT/$FIXTURE_BASE/src/lib_over.rs"
mkdir -p -- "$(dirname -- "$FIXTURE_OVER")"
{
    printf '// synthetic oversized fixture for size-guard self-test\n'
    printf '// padding row: %s\n' \
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    for i in 1 2 3 4 5 6 7 8 9 10; do
        printf 'fn line_%02d() { /* padding */ }\n' "$i"
    done
} > "$FIXTURE_OVER"

OVER_PATH="$FIXTURE_BASE/src/lib_over.rs"
set +e
run_guard t3_oversized files-from-stdin "$OVER_PATH"
t3_actual=$?
set -e
# The retirement contract, stated twice: an oversized file with NO marker must
# NOT fail (first assertion) and must still be named (second). Either one alone
# is satisfiable by a broken script — a reporter that prints nothing passes the
# first, and the pre-retirement gate passed the second.
assert_exit "T3 unacknowledged oversized file exits 0" 0 "$t3_actual" "$LOG_DIR/t3_oversized.log.out"
assert_output_contains "T3 ranks the unacknowledged fixture" \
    "unacknowledged  $OVER_PATH" "$LOG_DIR/t3_oversized.log.out"
assert_output_contains "T3 summary counts it as unacknowledged" \
    "1 unacknowledged" "$LOG_DIR/t3_oversized.log.out"
assert_output_contains "T3 emits a notice annotation, not an error" \
    "::notice file=$OVER_PATH,line=1" "$LOG_DIR/t3_oversized.log.out"
assert_output_lacks "T3 emits no error annotation" \
    "::error" "$LOG_DIR/t3_oversized.log.out"
assert_output_contains "T3 writes the ranked table to the step summary" \
    "| \`$OVER_PATH\` |" "$LOG_DIR/t3_oversized.summary.md"

# ── T4: generated paths and .gen.* files are skipped ─────────────────
# Both **/generated/** directory paths and *.gen.* file suffixes must be
# filtered out, even when the content would otherwise be oversized.
FIXTURE_GEN_DIR="$REPO_ROOT/$FIXTURE_BASE/src/generated/big.rs"
mkdir -p -- "$(dirname -- "$FIXTURE_GEN_DIR")"
{
    printf '// synthetic generated fixture (under generated/)\n'
    for i in 1 2 3 4 5 6 7 8 9 10; do
        printf 'fn line_%02d() { /* generated padding */ }\n' "$i"
    done
} > "$FIXTURE_GEN_DIR"

FIXTURE_GEN_FILE="$REPO_ROOT/$FIXTURE_BASE/src/lib.gen.rs"
{
    printf '// synthetic generated fixture (.gen.rs suffix)\n'
    for i in 1 2 3 4 5 6 7 8 9 10; do
        printf 'fn line_%02d() { /* generated padding */ }\n' "$i"
    done
} > "$FIXTURE_GEN_FILE"

GEN_DIR_PATH="$FIXTURE_BASE/src/generated/big.rs"
GEN_FILE_PATH="$FIXTURE_BASE/src/lib.gen.rs"
# Mix the two generated fixtures with a small, in-scope, non-oversized
# baseline file so the guard has at least one real candidate to count.
FIXTURE_OK="$REPO_ROOT/$FIXTURE_BASE/src/small.rs"
printf 'fn small() {}\n' > "$FIXTURE_OK"
OK_PATH="$FIXTURE_BASE/src/small.rs"

set +e
run_guard t4_generated files-from-stdin \
    "$GEN_DIR_PATH" "$GEN_FILE_PATH" "$OK_PATH"
t4_actual=$?
set -e
assert_exit "T4 generated paths exit 0" 0 "$t4_actual" "$LOG_DIR/t4_generated.log.out"
assert_output_contains "T4 counts only the non-generated file" \
    "Checked 1 Rust source file(s)" "$LOG_DIR/t4_generated.log.out"
assert_output_lacks "T4 does not report the generated-dir file" \
    "FAIL  $GEN_DIR_PATH" "$LOG_DIR/t4_generated.log.out"
assert_output_lacks "T4 does not report the .gen.rs file" \
    "FAIL  $GEN_FILE_PATH" "$LOG_DIR/t4_generated.log.out"

# ── T5: djinn:allow-oversize marker classifies, it no longer permits ──
# Nothing fails, so the marker grants nothing. It is still parsed because
# it is the author's on-the-record statement that the size is deliberate,
# which is the one axis that makes the ranking actionable: it separates
# `acknowledged` rows from `unacknowledged` ones.
FIXTURE_ALLOW="$REPO_ROOT/$FIXTURE_BASE/src/allowed.rs"
{
    printf '// djinn:allow-oversize\n'
    printf '// synthetic oversized fixture protected by escape-hatch marker\n'
    printf '// padding row: %s\n' \
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    for i in 1 2 3 4 5 6 7 8 9 10; do
        printf 'fn line_%02d() { /* padding */ }\n' "$i"
    done
} > "$FIXTURE_ALLOW"

ALLOW_PATH="$FIXTURE_BASE/src/allowed.rs"
set +e
run_guard t5_marker files-from-stdin "$ALLOW_PATH"
t5_actual=$?
set -e
assert_exit "T5 marker file exits 0" 0 "$t5_actual" "$LOG_DIR/t5_marker.log.out"
# Four leading spaces: the status column is left-padded to 14, so
# "acknowledged" + 2 pad + 2 separator distinguishes it from the 14-char
# "unacknowledged" + 2 separator that T3 asserts.
assert_output_contains "T5 ranks the marker file as acknowledged" \
    "acknowledged    $ALLOW_PATH" \
    "$LOG_DIR/t5_marker.log.out"
assert_output_contains "T5 summary counts 1 acknowledged, 0 unacknowledged" \
    "(0 unacknowledged, 1 acknowledged by djinn:allow-oversize)" \
    "$LOG_DIR/t5_marker.log.out"

# ── T6: full-tree mode over the REAL tree exits 0 ───────────────────
# Deliberately run with thresholds low enough that the real repository is
# saturated with oversized files. Under the pre-retirement gate this exited
# 1; report-only means it must exit 0 while still naming what it found.
set +e
cd "$REPO_ROOT" && env \
    MAX_LINES=1 \
    MAX_BYTES=1 \
    sh "$GUARD" --all > "$LOG_DIR/t6_all.log.out" 2>&1
t6_actual=$?
set -e
assert_exit "T6 --all over a saturated tree exits 0" 0 "$t6_actual" "$LOG_DIR/t6_all.log.out"
if grep -q "Checked [0-9][0-9]* Rust source file(s)" "$LOG_DIR/t6_all.log.out"; then
    pass "T6 --all mode scanned the server tree"
else
    fail "T6 --all mode scanned the server tree" \
        "no 'Checked N Rust source file(s)' line in:
$(cat "$LOG_DIR/t6_all.log.out")"
fi
# Annotations are capped so GitHub's 10-per-step render limit does not
# silently discard the tail of a 100+ row report.
t6_notices=$(grep -c '^::notice ' "$LOG_DIR/t6_all.log.out" || true)
if [ "$t6_notices" -le 10 ] && [ "$t6_notices" -gt 0 ]; then
    pass "T6 --all caps annotations at 10 (emitted $t6_notices)"
else
    fail "T6 --all caps annotations at 10" "emitted $t6_notices"
fi

# ── T7: the report is ranked worst-first by bytes ────────────────────
# A flat list of 100+ paths is not a report. The ordering is the product:
# the biggest file must be rank 1 regardless of the input order.
FIXTURE_BIG="$REPO_ROOT/$FIXTURE_BASE/src/bigger.rs"
FIXTURE_SMALL="$REPO_ROOT/$FIXTURE_BASE/src/smaller.rs"
{
    printf '// synthetic BIG fixture\n'
    for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
        printf 'fn big_%02d() { /* %s */ }\n' "$i" \
            "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    done
} > "$FIXTURE_BIG"
{
    printf '// synthetic SMALL-but-still-oversized fixture\n'
    for i in 1 2 3; do
        printf 'fn small_%02d() {}\n' "$i"
    done
} > "$FIXTURE_SMALL"

BIG_PATH="$FIXTURE_BASE/src/bigger.rs"
SMALL_PATH="$FIXTURE_BASE/src/smaller.rs"
set +e
# Feed the SMALLER file first: rank must come from measurement, not order.
run_guard t7_rank files-from-stdin "$SMALL_PATH" "$BIG_PATH"
t7_actual=$?
set -e
assert_exit "T7 ranked report exits 0" 0 "$t7_actual" "$LOG_DIR/t7_rank.log.out"
t7_first=$(grep -E "^ +[0-9]+ +[0-9]+ +[0-9]+ +unacknowledged" "$LOG_DIR/t7_rank.log.out" \
    | head -n 1 | awk '{print $NF}')
if [ "$t7_first" = "$BIG_PATH" ]; then
    pass "T7 the largest file ranks first despite input order"
else
    fail "T7 the largest file ranks first despite input order" \
        "rank 1 was '$t7_first', expected '$BIG_PATH'
output:
$(cat "$LOG_DIR/t7_rank.log.out")"
fi
assert_output_contains "T7 still names the smaller offender" \
    "unacknowledged  $SMALL_PATH" "$LOG_DIR/t7_rank.log.out"

# ── summary ──────────────────────────────────────────────────────────
printf -- '------------------------------------------\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
exit 0
