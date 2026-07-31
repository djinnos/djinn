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
#     the ban perfectly by having no predicate left at all;
#   * (0ppk-1c) a tree whose only `with_resize_authority` caller is a test, or
#     which has none at all -- the shape `0ppk-1a` shipped in, where the whole
#     resize stack was merged, green, and structurally unable to move a Pod.
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
LIFT="$REL_DIR/resize_lift.rs"
LIFT_TESTS="$REL_DIR/resize_lift_tests.rs"
# The production composition root the arming check scans. Spelled in halves so
# this harness cannot satisfy the guard's own search by containing the symbol in
# a comment.
ARMING_DIR=server/src/server/state
ARMING_FILE="$ARMING_DIR/mod.rs"
ARMING_SYMBOL="with_resize""_authority"

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
    cat >"$SCRATCH/$LIFT" <<'EOF'
// A clean lift module.
EOF
    cat >"$SCRATCH/$LIFT_TESTS" <<'EOF'
// A clean lift test file.
EOF
    mkdir -p -- "$SCRATCH/$ARMING_DIR"
    # A production composition site that arms the authority, above the file's
    # test module -- the shape the guard must accept.
    cat >"$SCRATCH/$ARMING_FILE" <<EOF
fn new_inner() {
    let _ = BuildLeaseService::new(repo, cap).$ARMING_SYMBOL(authority);
}

#[cfg(test)]
mod tests {}
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

# ── 0ppk-1c: the arming check ──────────────────────────────────────────────

fixture
cat >"$SCRATCH/$ARMING_FILE" <<'EOF'
fn new_inner() {
    let _ = BuildLeaseService::new(repo, cap);
}
EOF
expect 1 "deleting the production arming call fails, naming the symbol"

fixture
# Observed while building this guard: the first implementation passed on a
# COMMENTED-OUT arming call, which is exactly the mutation the criterion names.
cat >"$SCRATCH/$ARMING_FILE" <<EOF
fn new_inner() {
    let _ = BuildLeaseService::new(repo, cap)
        ;//.$ARMING_SYMBOL(authority)
}
EOF
expect 1 "a COMMENTED-OUT arming call is not a caller"

fixture
cat >"$SCRATCH/$ARMING_FILE" <<EOF
fn new_inner() {
    /*
     * .$ARMING_SYMBOL(authority)
     */
    let _ = BuildLeaseService::new(repo, cap);
}
EOF
expect 1 "an arming call inside a block comment is not a caller"

fixture
# The ONLY caller is inside a test module. This is precisely the false positive
# the check exists to reject: a unit test that installs the authority itself
# proves the setter works and proves nothing about reachability.
cat >"$SCRATCH/$ARMING_FILE" <<EOF
fn new_inner() {
    let _ = BuildLeaseService::new(repo, cap);
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_can_be_armed() {
        let _ = BuildLeaseService::new(repo, cap).$ARMING_SYMBOL(authority);
    }
}
EOF
expect 1 "an arming call that exists ONLY inside a test module fails"

fixture
# A `*_tests.rs` sidecar is excluded by filename, for the same reason.
cat >"$SCRATCH/$ARMING_FILE" <<'EOF'
fn new_inner() {
    let _ = BuildLeaseService::new(repo, cap);
}
EOF
cat >"$SCRATCH/$ARMING_DIR/composition_tests.rs" <<EOF
fn t() { BuildLeaseService::new(repo, cap).$ARMING_SYMBOL(authority); }
EOF
expect 1 "an arming call that exists ONLY in a *_tests.rs file fails"

fixture
rm -rf -- "$SCRATCH/server/src"
expect 1 "a missing composition root fails rather than passing vacuously"

fixture
# An INDENTED `#[cfg(test)]` is a field or statement attribute, not a test
# module. It must not truncate the scan and hide the production caller that
# follows it -- which is exactly what the real `AppState` file looks like.
cat >"$SCRATCH/$ARMING_FILE" <<EOF
struct Inner {
    #[cfg(test)]
    test_bypass_persist: bool,
}

fn new_inner() {
    let _ = BuildLeaseService::new(repo, cap).$ARMING_SYMBOL(authority);
}
EOF
expect 0 "an indented #[cfg(test)] field does not hide the production caller"

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
