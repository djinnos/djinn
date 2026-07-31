#!/bin/sh
# Self-test harness for scripts/check-resize-authorization-boundary.sh.
#
# A guard nobody has watched fail is not a guard. This drives the production
# guard against synthetic trees under a scratch directory (always torn down) via
# its GUARD_ROOT hook, and proves it fires on each of the three failures it
# claims to catch:
#
#   * a banned retired-relation token, in CODE;
#   * the same token in a COMMENT (the raw-SQL guard's lesson: text guards are
#     comment-blind unless you decide otherwise, and here we decide they are not);
#   * a deleted guarded file, which must fail rather than pass vacuously;
#   * a module that stopped naming a required type, which would otherwise satisfy
#     the ban perfectly by having no predicate left at all.
#
# Run from anywhere:
#
#   sh scripts/test-check-resize-authorization-boundary.sh
#
# Exits 0 on success, 1 if any assertion failed. Pure POSIX shell; no cargo.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GUARD="$SCRIPT_DIR/check-resize-authorization-boundary.sh"
SCRATCH="$REPO_ROOT/.resize-authorization-guard-selftest"
REL_DIR=server/crates/djinn-coordinator/src
MODULE="$REL_DIR/resize_authorization.rs"
TESTS="$REL_DIR/resize_authorization_tests.rs"

cleanup() {
    rm -rf -- "$SCRATCH" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

if [ ! -f "$GUARD" ]; then
    printf 'FATAL: production guard not found at %s\n' "$GUARD" >&2
    exit 2
fi

PASS=0
FAIL=0

pass() {
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
}

fail() {
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n' "$1" >&2
}

# Build a clean, passing fixture tree from scratch.
fixture() {
    rm -rf -- "$SCRATCH"
    mkdir -p -- "$SCRATCH/$REL_DIR"
    cat >"$SCRATCH/$MODULE" <<'EOF'
// A clean module: it names the required types and no retired storage.
use djinn_supervisor::services::InvocationLiftDecision;
use djinn_launcher_protocol::LauncherAuthorityProtocol;
EOF
    cat >"$SCRATCH/$TESTS" <<'EOF'
// A clean test file.
EOF
}

# Run the guard against the scratch tree; echo its exit code.
run_guard() {
    set +e
    GUARD_ROOT="$SCRATCH" "$GUARD" >/dev/null 2>&1
    code=$?
    set -e
    printf '%s' "$code"
}

expect() {
    want=$1
    label=$2
    got=$(run_guard)
    if [ "$got" = "$want" ]; then
        pass "$label"
    else
        fail "$label (expected exit $want, got $got)"
    fi
}

printf 'Self-testing %s\n' "$GUARD"

fixture
expect 0 "a clean module passes"

fixture
printf 'let row = read("admission_handoff");\n' >>"$SCRATCH/$MODULE"
expect 1 "a retired-relation read in CODE fails"

fixture
printf '// see the admission_handoff row for context\n' >>"$SCRATCH/$MODULE"
expect 1 "a retired-relation mention in a COMMENT fails"

fixture
printf 'let e = row.emergency_ack_epoch;\n' >>"$SCRATCH/$MODULE"
expect 1 "a retired companion column fails"

fixture
printf '// the invocation_ack_epoch is current\n' >>"$SCRATCH/$TESTS"
expect 1 "the guard covers the TEST file too"

fixture
rm -f -- "$SCRATCH/$MODULE"
expect 1 "a deleted guarded file fails rather than passing vacuously"

fixture
cat >"$SCRATCH/$MODULE" <<'EOF'
// The predicate was deleted. No banned token, and no predicate either.
EOF
expect 1 "a module that no longer names the required types fails"

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
