#!/bin/sh
# Self-test harness for scripts/check-include-macro.sh.
#
# Drives the production guard through --files-from-stdin against synthetic
# fixture files this script creates (and always tears down) under a scratch
# directory inside the repo. Pure POSIX shell; no cargo, no network.
#
# A guard nobody has watched fail is not a guard. Every assertion below that
# expects exit 1 exists to prove this one actually fires — including on the
# exact shape it was written for (`include!("f.rs")`), on the `.inc` variant,
# and on whitespace-padded and indented forms.
#
# Run from anywhere:
#
#   sh scripts/test-check-include-macro.sh
#
# Exits 0 on success, 1 if any assertion failed.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GUARD="$SCRIPT_DIR/check-include-macro.sh"
FIXTURE_DIR="$REPO_ROOT/server/crates/djinn_include_guard_fixture"

cleanup() {
    rm -rf -- "$FIXTURE_DIR" 2>/dev/null || true
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
    if [ -n "${2:-}" ]; then
        printf '       %s\n' "$2" >&2
    fi
}

cleanup
mkdir -p "$FIXTURE_DIR"

# write_fixture <name> <line>...
write_fixture() {
    name=$1
    shift
    : >"$FIXTURE_DIR/$name"
    for line in "$@"; do
        printf '%s\n' "$line" >>"$FIXTURE_DIR/$name"
    done
}

# expect <exit-code> <label> <fixture-name>...
#
# Pipes the named fixtures into the guard and asserts its exit status.
expect() {
    want=$1
    label=$2
    shift 2

    list=""
    for f in "$@"; do
        list="$list$FIXTURE_DIR/$f
"
    done

    set +e
    out=$(printf '%s' "$list" | "$GUARD" --files-from-stdin 2>&1)
    got=$?
    set -e

    if [ "$got" -eq "$want" ]; then
        pass "$label (exit $got)"
    else
        fail "$label" "expected exit $want, got $got; output: $out"
    fi
}

printf '== running self-tests for scripts/check-include-macro.sh ==\n'

# ---------------------------------------------------------------------------
# Non-vacuity: the guard must FAIL on every form of the thing it bans.
# ---------------------------------------------------------------------------

write_fixture violation_rs.rs \
    '#[cfg(test)]' \
    'mod tests {' \
    '    include!("helpers.rs");' \
    '}'
expect 1 'flags include! of a .rs fragment' violation_rs.rs

write_fixture violation_inc.rs \
    'include!("tests_part1.inc");'
expect 1 'flags include! of a .inc fragment' violation_inc.rs

write_fixture violation_spaced.rs \
    'include! ( "helpers.rs" ) ;'
expect 1 'flags whitespace-padded include!' violation_spaced.rs

write_fixture violation_trailing.rs \
    'fn f() {}' \
    'include!("a.rs"); // pull in the rest' \
    'fn g() {}'
expect 1 'flags include! with a trailing comment' violation_trailing.rs

write_fixture violation_manifest_dir.rs \
    'include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/extra.rs"));'
expect 1 'CARGO_MANIFEST_DIR is NOT the build-script exemption' violation_manifest_dir.rs

write_fixture violation_concat_only.rs \
    'include!(concat!("a", "b", ".rs"));'
expect 1 'concat! alone does not earn the exemption' violation_concat_only.rs

# The marker the sibling size guard uses. It must NOT work here: this rule has
# no opt-out, and a fixture proving so is the only thing that keeps a future
# edit from quietly adding one.
write_fixture violation_marker.rs \
    '// djinn:allow-oversize' \
    '// djinn:allow-include' \
    '#[allow(clippy::all)]' \
    'include!("helpers.rs"); // djinn:allow-include' \
    ''
expect 1 'no comment marker suppresses the guard' violation_marker.rs

# ---------------------------------------------------------------------------
# No false positives.
# ---------------------------------------------------------------------------

write_fixture clean_mod.rs \
    '#[cfg(test)]' \
    'mod helpers;' \
    'fn f() { let _ = include_str!("data.txt"); }' \
    'fn g() { let _ = include_bytes!("data.bin"); }'
expect 0 'mod declarations and include_str!/include_bytes! are fine' clean_mod.rs

# The real shape at server/crates/djinn-db/tests/effective_creator_rollout.rs:275
# — a test that BUILDS the string "include!(...)" to assert on it. Stripping
# string literals before scanning is what keeps this from being flagged.
write_fixture clean_string_literal.rs \
    'fn f(file_name: &str) -> String {' \
    '    let include = format!("include!(\"{file_name}\")");' \
    '    include' \
    '}'
expect 0 'include! inside a string literal is not a violation' clean_string_literal.rs

write_fixture clean_comment.rs \
    '// Historically this module used include!("part1.inc"); it now uses mod.' \
    '/// Do not reintroduce include!(...) here.' \
    'mod part1;'
expect 0 'include! mentioned only in comments is not a violation' clean_comment.rs

write_fixture clean_out_dir.rs \
    'include!(concat!(env!("OUT_DIR"), "/generated.rs"));'
expect 0 'build-script OUT_DIR pattern is exempt' clean_out_dir.rs

write_fixture clean_out_dir_spaced.rs \
    'include!( concat!( env!( "OUT_DIR" ), "/bindings.rs" ) );'
expect 0 'build-script OUT_DIR pattern is exempt when spaced' clean_out_dir_spaced.rs

expect 0 'empty file list passes' ''

# ---------------------------------------------------------------------------
# Mixed batch: one bad file among good ones still fails.
# ---------------------------------------------------------------------------

expect 1 'a single violation in a mixed batch fails the run' \
    clean_mod.rs clean_out_dir.rs violation_rs.rs clean_comment.rs

# ---------------------------------------------------------------------------
# The repository itself must be clean.
# ---------------------------------------------------------------------------

set +e
repo_out=$("$GUARD" 2>&1)
repo_status=$?
set -e
if [ "$repo_status" -eq 0 ]; then
    pass 'repository-wide scan is clean'
else
    fail 'repository-wide scan is clean' "$repo_out"
fi

# Usage errors are distinguishable from violations (exit 2, not 1).
set +e
"$GUARD" --nonsense >/dev/null 2>&1
usage_status=$?
set -e
if [ "$usage_status" -eq 2 ]; then
    pass 'unknown argument exits 2, not 1'
else
    fail 'unknown argument exits 2, not 1' "got $usage_status"
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
