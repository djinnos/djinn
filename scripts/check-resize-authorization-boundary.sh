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

if [ "$status" -eq 0 ]; then
    echo "OK: resize authorization depends on the lift-decision types, not on retired storage."
fi

exit "$status"
