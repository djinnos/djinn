#!/bin/sh
# Self-test for the shared Rust source-text scanner.
#
# WHY THIS FILE EXISTS
#
# `scripts/lib/rust-source-scan.awk` is now the single comment-stripping and
# `#[cfg(test)]`-tracking implementation behind five guards. A silent
# regression in it would take all five down at once, and the failure would be
# invisible: a guard that stops matching anything still exits 0.
#
# So every case below is written as a PAIR. For each thing the scanner must
# ignore there is an adjacent thing it must still catch, because "ignores
# comments" and "ignores everything" are indistinguishable from a green run.
# That pairing is the whole point — the same reasoning as
# `banned_patch_variant_in` in server/tests/task_run_resize_kind.rs (#2871).
#
# Two traps this file is written against:
#
#   * A PRESENCE assertion and a BAN assertion need DIFFERENT mutations.
#     Swapping a required token for another valid one exercises the presence
#     arm and proves nothing about the ban. To test a ban you must ADD the
#     banned token while LEAVING the required one in place.
#   * A trailing comment must not launder the code in front of it.
#     `foo(); // banned` is still a call to `foo()`.
#
# Usage: sh scripts/test-rust-source-scan.sh
# Exit codes: 0 all cases pass, 1 a case failed.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SCAN="$SCRIPT_DIR/lib/rust-source-scan.awk"

if [ ! -f "$SCAN" ]; then
    echo "FATAL: shared scanner not found: $SCAN" >&2
    exit 2
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/rust-source-scan-selftest.XXXXXX")
trap 'rm -rf -- "$WORK"' EXIT INT TERM

PASS=0
FAIL=0

# scan <pattern> [strings] [scope] [force_test] -> matched line numbers, space-separated
scan() {
    RS_PATTERN="$1" awk -f "$SCAN" \
        -v strings="${2:-keep}" \
        -v scope="${3:-any}" \
        -v force_test="${4:-0}" \
        "$WORK/fixture.rs" | cut -d: -f2 | tr '\n' ' ' | sed 's/ $//'
}

# expect <label> <expected-lines> <pattern> [strings] [scope] [force_test]
expect() {
    _label=$1
    _want=$2
    shift 2
    _got=$(scan "$@")
    if [ "$_got" = "$_want" ]; then
        PASS=$((PASS + 1))
        printf '  ok   %s\n' "$_label"
    else
        FAIL=$((FAIL + 1))
        printf '  FAIL %s\n' "$_label" >&2
        printf '       expected lines [%s], got [%s]\n' "$_want" "$_got" >&2
    fi
}

printf 'rust-source-scan self-test\n'

# ── 1. Comments are not code, in BOTH directions ───────────────────────
#
# Line 1 is the pcod shape: a comment written to explain why a token is wrong,
# which tripped the ban that forbids it. Line 5 is its load-bearing pair — if
# the stripper were "drop any line mentioning the token", line 5 would vanish
# with line 1 and the guard would be worthless.
cat >"$WORK/fixture.rs" <<'EOF'
// Never use Patch::Apply here; it clobbers the whole object.
/// See [`Patch::Apply`] for why this is wrong.
/* A block comment naming Patch::Apply is prose too. */
pub fn patch() {
    let p = Patch::Apply(body);
}
EOF
expect "a line comment naming the banned token is not a match" "5" 'Patch::Apply'

# ── 2. A trailing comment does not launder the code in front of it ─────
cat >"$WORK/fixture.rs" <<'EOF'
// forbidden_call() in a comment
let x = 1; // forbidden_call()
forbidden_call(); // TODO: remove
EOF
expect "code before a trailing comment is still code" "3" 'forbidden_call\('

# ── 3. A URL in a string literal does not truncate the line ────────────
#
# The naive fix for (1) is `sub(/\/\/.*/, "", line)`. That turns
# `let u = "https://x"; banned();` into `let u = "https:` and drops a real
# violation — a FALSE NEGATIVE introduced by fixing a false positive.
cat >"$WORK/fixture.rs" <<'EOF'
let doc = "https://example.com/api";
let u = "https://example.com"; banned_call();
// https://example.com banned_call()
EOF
expect "a // inside a string literal does not start a comment" "2" 'banned_call\('

# ── 4. strings=blank hides literal DATA but not calls built around it ──
#
# The pair matters: a scanner that detects SQL in source text feeds Rust source
# to itself AS A STRING, and the guard was matching its own fixtures. But
# `sqlx::query(&format!("..."))` still has the call outside every literal.
cat >"$WORK/fixture.rs" <<'EOF'
let hits = scan_sql("sqlx::query!(\"INSERT INTO t\")");
sqlx::query(&format!("{SET}owner = $3"));
EOF
expect "strings=blank ignores a literal payload" "2" 'sqlx::query[!(]' blank
expect "strings=keep sees inside the literal" "1 2" 'sqlx::query[!(]' keep

# A char literal holding a double quote must not open a phantom string and
# swallow the real call after it.
cat >"$WORK/fixture.rs" <<'EOF'
let ch = '"'; banned_call();
let esc = '\"'; banned_call();
EOF
expect "a '\"' char literal does not hide the call after it" "1 2" 'banned_call\(' blank

# ── 5. PRODUCTION CODE AFTER A #[cfg(test)] BLOCK IS PRODUCTION CODE ───
#
# The check-resize-reachability.sh blind spot. Truncating at the first marker
# meant the guard read the first 8% of a 4147-line file and never saw the
# composition site it existed to find.
cat >"$WORK/fixture.rs" <<'EOF'
pub fn before() { arm_it(); }

#[cfg(test)]
mod tests {
    #[test]
    fn decoy() { arm_it(); }
}

pub fn after() { arm_it(); }
EOF
expect "production callers on both sides of a test mod are found" "1 9" 'arm_it\(' blank prod
expect "the caller inside the test mod is excluded" "6" 'arm_it\(' blank test

# ── 6. #[cfg(test)] on a FIELD introduces no block ─────────────────────
#
# A field ends in `,`, not `{` or `;`. A tracker that arms on the attribute and
# waits for the next `{` swallows the following production function whole. This
# is the live shape in server/src/server/state/mod.rs from line 342.
cat >"$WORK/fixture.rs" <<'EOF'
pub struct AppState {
    /// Test-only bypass.
    #[cfg(test)]
    test_bypass: bool,
    real: u32,
}

pub fn compose() {
    arm_it();
}
EOF
expect "a #[cfg(test)] field does not swallow the next fn" "9" 'arm_it\(' blank prod
expect "and nothing in that file counts as test code" "" 'arm_it\(' blank test

# `#[cfg(test)] mod x;` and `#[cfg(test)] use ...;` open no block either.
cat >"$WORK/fixture.rs" <<'EOF'
#[cfg(test)]
mod slow_tests;

#[cfg(test)]
use std::io::Write;

pub fn compose() { arm_it(); }
EOF
expect "a bodyless #[cfg(test)] item does not swallow the next fn" "7" 'arm_it\(' blank prod

# ── 7. A multi-line test signature still resolves to its block ─────────
#
# The mirror hazard of case 6: disarm too eagerly and a `#[tokio::test] async
# fn foo(\n a: A,\n) {` body is classified as production, so a test-only hit is
# reported as a production one.
cat >"$WORK/fixture.rs" <<'EOF'
#[tokio::test]
async fn reads(
    fixture: Fixture,
    other: Other,
) {
    arm_it();
}

pub fn compose() { arm_it(); }
EOF
expect "a multi-line #[test] signature keeps its body in test scope" "9" 'arm_it\(' blank prod
expect "and the body itself is test scope" "6" 'arm_it\(' blank test

# Stacked attributes between the marker and the item.
cat >"$WORK/fixture.rs" <<'EOF'
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    fn helper() { arm_it(); }
}

pub fn compose() { arm_it(); }
EOF
expect "stacked attributes do not break the tracker" "7" 'arm_it\(' blank prod

# ── 8. force_test=1 for a module a PARENT declares as #[cfg(test)] ─────
#
# The attribute lives in the parent file, so nothing in this file can see it.
cat >"$WORK/fixture.rs" <<'EOF'
fn helper() {
    arm_it();
}
EOF
expect "force_test=1 makes the whole file test scope" "" 'arm_it\(' blank prod 1
expect "force_test=1 still reports it as a test hit" "2" 'arm_it\(' blank test 1

# ── 9. A commented-out call is not a call ──────────────────────────────
#
# The 0vku shape: a CI guard passed on a commented-out arming call, which was
# the exact mutation its acceptance criterion named.
cat >"$WORK/fixture.rs" <<'EOF'
pub fn compose() {
    // arm_it();
    let _ = build();
}
EOF
expect "a commented-out call does not satisfy a presence scan" "" 'arm_it\(' blank prod

# ── 10. State resets between files ─────────────────────────────────────
#
# An unterminated block comment in one file must not blind the scanner to the
# next one.
cat >"$WORK/fixture.rs" <<'EOF'
pub fn compose() { arm_it(); }
EOF
cat >"$WORK/leading.rs" <<'EOF'
/* unterminated block comment
EOF
_got=$(RS_PATTERN='arm_it\(' awk -f "$SCAN" -v strings=blank "$WORK/leading.rs" "$WORK/fixture.rs" | wc -l | tr -d ' ')
if [ "$_got" = "1" ]; then
    PASS=$((PASS + 1))
    printf '  ok   block-comment state resets at each file\n'
else
    FAIL=$((FAIL + 1))
    printf '  FAIL block-comment state resets at each file (got %s hits, want 1)\n' "$_got" >&2
fi

# ── 11. Misconfiguration is an error, not a silent pass ────────────────
#
# A guard whose scanner quietly matched nothing would report OK forever.
set +e
awk -f "$SCAN" -v strings=blank "$WORK/fixture.rs" >/dev/null 2>&1
_rc=$?
set -e
if [ "$_rc" -eq 2 ]; then
    PASS=$((PASS + 1))
    printf '  ok   a missing RS_PATTERN exits 2 rather than matching nothing\n'
else
    FAIL=$((FAIL + 1))
    printf '  FAIL a missing RS_PATTERN should exit 2, got %s\n' "$_rc" >&2
fi

set +e
RS_PATTERN='x' awk -f "$SCAN" -v scope=production "$WORK/fixture.rs" >/dev/null 2>&1
_rc=$?
set -e
if [ "$_rc" -eq 2 ]; then
    PASS=$((PASS + 1))
    printf '  ok   an unknown scope exits 2 rather than defaulting\n'
else
    FAIL=$((FAIL + 1))
    printf '  FAIL an unknown scope should exit 2, got %s\n' "$_rc" >&2
fi

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
