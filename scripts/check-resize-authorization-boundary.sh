#!/bin/sh
# Resize-authorization storage-independence guard (0ppk-1a, acceptance criterion 3).
#
# WHY THIS EXISTS
#
# Proposal 9oga's `flc5` (task `0rld`) will DROP the retired admission-handoff
# relations. Slice S3b already re-homed the invocation lease's arming authority
# onto `InvocationLeaseAuthorityRepository`, and every 3i92 task has been specced
# against the TYPES -- `InvocationLiftDecision`, and the `leaf-v1` / `resize-v2`
# `LauncherAuthorityProtocol` -- rather than against the storage, precisely so
# this code survives that DROP without a migration-day edit.
#
# Nothing about that produces a compile error if it stops being true. A single
# convenience read of the retired relation added to the authorization module
# would compile, pass, and then take production's resize authorization down on
# the day the DROP migration runs. Text is exactly the right granularity for the
# assertion, because the failure is "this module names that relation at all".
#
# WHAT IS CHECKED
#
#   1. The guarded files exist. A guard whose subject was deleted or renamed
#      must fail loudly rather than pass vacuously.
#   2. No guarded file contains a banned token (the retired relation and its
#      retired companion columns), in code OR in a comment. A comment that names
#      it is a comment that invites a reader to read it.
#   3. Each required type is still named by the module. Deleting the predicate
#      would otherwise satisfy check 2 perfectly.
#   4. (0ppk-1c) `with_resize_authority` has at least one PRODUCTION caller.
#
# WHY CHECK 4
#
# `BuildLeaseService` holds its resize authorization as
# `Option<Arc<ResizeAuthority>>`. For the whole of `0ppk-1a` that `Option` was
# `None` at every composition and `with_resize_authority` had zero call sites
# outside tests: the authorization layer, the clamp, and their entire test suite
# were merged, green, and structurally unable to move a Pod. Nothing about that
# produces a compile error, and no unit test can catch it — a unit test that
# installs the authority itself proves only that the setter works.
#
# Reachability is a property of the COMPOSITION SITE, so it is checked here, by
# text, over the production tree. `#[cfg(test)]` blocks, `*_tests.rs` files and
# `.worktrees/` scratch copies are excluded deliberately: a caller in any of
# those is exactly the false positive that would let the arming be deleted while
# CI stayed green.
#
# Usage:
#   ./scripts/check-resize-authorization-boundary.sh
#   GUARD_ROOT=/path/to/fixture ./scripts/check-resize-authorization-boundary.sh
#     (self-test hook; see scripts/test-check-resize-authorization-boundary.sh)

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)
ROOT=${GUARD_ROOT:-$REPO_ROOT}

cd "$ROOT"

GUARDED_FILES="
server/crates/djinn-coordinator/src/resize_authorization.rs
server/crates/djinn-coordinator/src/resize_authorization_tests.rs
server/crates/djinn-coordinator/src/resize_lift.rs
server/crates/djinn-coordinator/src/resize_lift_tests.rs
"

# The retired handoff relation and the protocol columns flc5 drops with it.
BANNED_TOKENS="
admission_handoff
admission_handoff_generation_ack
emergency_ack_epoch
invocation_ack_epoch
"

# The types the should-we-lift-at-all predicate must be written against.
REQUIRED_TYPES="
InvocationLiftDecision
LauncherAuthorityProtocol
"

# Only the module itself must name the types; the test file is free not to.
TYPED_FILE=server/crates/djinn-coordinator/src/resize_authorization.rs

SCAN_AWK="$SCRIPT_DIR/lib/rust-source-scan.awk"
if [ ! -f "$SCAN_AWK" ]; then
    echo "FAIL: missing shared scanner: $SCAN_AWK" >&2
    exit 2
fi

# Every text assertion below that is about CODE goes through the shared
# scanner in scripts/lib/rust-source-scan.awk, which strips comments in a
# string-literal-aware pass and tracks `#[cfg(test)]` structurally.
#
#   rust_hits <file> <ERE> [scope]   -> `<file>:<line>:<text>` per match
#
# `strings=blank` erases literal bodies, so a type or symbol named inside a
# string cannot satisfy a presence assertion either.
rust_hits() {
    RS_PATTERN="$2" awk -f "$SCAN_AWK" -v strings=blank -v scope="${3:-any}" "$1" 2>/dev/null
}

status=0

for file in $GUARDED_FILES; do
    if [ ! -f "$file" ]; then
        echo "FAIL: guarded file is missing: $file" >&2
        echo "      The resize-authorization boundary guard has no subject. If the" >&2
        echo "      module moved, update GUARDED_FILES in $0." >&2
        status=1
        continue
    fi
    for token in $BANNED_TOKENS; do
        if grep -n -- "$token" "$file" >/dev/null 2>&1; then
            echo "FAIL: $file references the retired relation token '$token':" >&2
            grep -n -- "$token" "$file" >&2
            echo "      This module must depend on the lift-decision TYPES, never on" >&2
            echo "      storage flc5 (task 0rld) is going to DROP." >&2
            status=1
        fi
    done
done

# Check 3 asks whether the module still names each required type IN CODE.
#
# It used to be a bare `grep`, which is the 1j64 defect: `resize_authorization.rs`
# names both types in doc comments as well as in code (`/// That predicate is
# [`InvocationLiftDecision`] …`), so deleting every real use would have left the
# guard green on the prose that describes the use. A PRESENCE assertion is the
# silent direction — it does not fail, it just stops meaning anything.
if [ -f "$TYPED_FILE" ]; then
    for type_name in $REQUIRED_TYPES; do
        if [ -z "$(rust_hits "$TYPED_FILE" "$type_name")" ]; then
            echo "FAIL: $TYPED_FILE no longer names '$type_name' in code." >&2
            echo "      The should-we-lift-at-all predicate must be written against" >&2
            echo "      that type. Removing it satisfies the ban above vacuously," >&2
            echo "      and a doc comment that merely mentions the type does not" >&2
            echo "      count -- that is how a presence guard goes quiet." >&2
            status=1
        fi
    done
fi

# ── Check 4: the arming has a production caller ────────────────────────────
#
# The symbol is spelled in two halves so this script's own text cannot satisfy
# the search it performs — a guard that matches itself is a guard that can never
# fail.
ARMING_SYMBOL="with_resize""_authority"
ARMING_ROOT=server/src

# A hit counts only if it is production code: outside every `#[cfg(test)]`
# block and outside every `#[test]`-attributed item.
#
# THIS USED TO TRUNCATE AT THE FIRST UNINDENTED `#[cfg(test)]`, and that was
# wrong for a reason worth recording, because the sibling guard
# `scripts/check-resize-reachability.sh` was taken down by the same idea in a
# stronger form the same week. The old rationale here read: "test modules go at
# the end of a file in this codebase, so 'before the first top-level
# `#[cfg(test)]`' is exactly 'outside every test module'". That convention is
# not a rule and nothing enforces it. `server/src/server/state/mod.rs` -- the
# very file this check exists to read -- is 4147 lines with `#[cfg(test)]`
# FIELD attributes from line 342 onward; requiring the marker to be unindented
# is the only reason this check ever saw line 540, where the arming actually
# lives. One production item moved below a test module and the guard would have
# stopped seeing the composition site: the single place its own purpose
# requires it to look.
#
# "Fails closed" was the defence, and it is not good enough. A guard that
# cannot see its subject is not safe, it is decorative: the reachability guard
# failed closed too, and the effect was that it verified reachability against
# the first 8% of a file. So the test-context rule is now STRUCTURAL --
# brace-matched blocks, `#[cfg(test)]` on a field or a `mod x;` correctly
# treated as introducing no block at all -- and lives in the shared scanner at
# scripts/lib/rust-source-scan.awk, which is self-tested in both directions by
# scripts/test-rust-source-scan.sh.
#
# A commented-out call is still not a call: the scanner strips comments before
# matching, so `// .with_resize_authority(x)` does not count. Without that, the
# named failing mutation "delete the arming" is satisfied by commenting it out
# and the guard passes on a composition that arms nothing -- which is what the
# first version of this check actually did.
arming_callers() {
    [ -d "$ARMING_ROOT" ] || return 0
    find "$ARMING_ROOT" -name '*.rs' -type f 2>/dev/null |
        grep -v -- '_tests\.rs$' |
        grep -v -- '/\.worktrees/' |
        while IFS= read -r file; do
            rust_hits "$file" "\\.$ARMING_SYMBOL\\(" prod
        done
}

if [ ! -d "$ARMING_ROOT" ]; then
    echo "FAIL: $ARMING_ROOT does not exist; the arming guard has no subject." >&2
    echo "      If the server composition root moved, update ARMING_ROOT in $0." >&2
    status=1
elif [ -z "$(arming_callers)" ]; then
    echo "FAIL: no production caller of \`$ARMING_SYMBOL\` under $ARMING_ROOT." >&2
    echo "      \`BuildLeaseService\` holds its resize authorization as an" >&2
    echo "      Option that defaults to None. With no production caller the" >&2
    echo "      whole resize stack is merged, green, and structurally unable" >&2
    echo "      to move a Pod -- which is the exact state 0ppk-1a shipped in" >&2
    echo "      and 0ppk-1c exists to end. Re-arm it in AppState::new_inner." >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "OK: resize authorization depends on the lift-decision types, not on retired storage."
    echo "OK: the resize authorization is armed from production code:"
    arming_callers | sed 's/^/    /'
fi

exit "$status"
