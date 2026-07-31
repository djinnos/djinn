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

if [ -f "$TYPED_FILE" ]; then
    for type_name in $REQUIRED_TYPES; do
        if ! grep -n -- "$type_name" "$TYPED_FILE" >/dev/null 2>&1; then
            echo "FAIL: $TYPED_FILE no longer names '$type_name'." >&2
            echo "      The should-we-lift-at-all predicate must be written against" >&2
            echo "      that type. Removing it satisfies the ban above vacuously." >&2
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

# A hit counts only if it precedes the file's first UNINDENTED `#[cfg(test)]`.
#
# An unindented `#[cfg(test)]` is a test-MODULE attribute; the indented ones are
# `#[cfg(test)]` struct fields and statements, which sit in production code and
# must not truncate the scan. Test modules go at the end of a file in this
# codebase, so "before the first top-level `#[cfg(test)]`" is exactly "outside
# every test module" without this script having to parse Rust.
#
# The rule is conservative in the safe direction: a caller appearing after one
# is IGNORED, so the guard can only ever under-count production callers, never
# over-count them. It can fail spuriously; it cannot pass spuriously.
arming_callers() {
    [ -d "$ARMING_ROOT" ] || return 0
    find "$ARMING_ROOT" -name '*.rs' -type f 2>/dev/null |
        grep -v -- '_tests\.rs$' |
        grep -v -- '/\.worktrees/' |
        while IFS= read -r file; do
            awk -v sym="$ARMING_SYMBOL" -v path="$file" '
                /^#\[cfg\(test\)\]/ { exit }
                # A commented-out call is not a call. Without this, the named
                # mutation "delete the arming" is satisfied by commenting it out
                # and the guard passes on a composition that arms nothing --
                # which is what the first version of this check actually did.
                {
                    raw = $0
                    code = $0
                    if (in_block) {
                        if (match(code, /\*\//)) {
                            code = substr(code, RSTART + RLENGTH)
                            in_block = 0
                        } else {
                            next
                        }
                    }
                    while (match(code, /\/\*/)) {
                        head = substr(code, 1, RSTART - 1)
                        rest = substr(code, RSTART + 2)
                        if (match(rest, /\*\//)) {
                            code = head substr(rest, RSTART + RLENGTH)
                        } else {
                            code = head
                            in_block = 1
                            break
                        }
                    }
                    sub(/\/\/.*/, "", code)
                    if (index(code, "." sym "(")) {
                        printf "%s:%d:%s\n", path, NR, raw
                    }
                }
            ' "$file"
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
