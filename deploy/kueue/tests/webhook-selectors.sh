#!/usr/bin/env bash
# Structural regression contract for the scoped vendored Kueue webhook asset.
# Usage: bash deploy/kueue/tests/webhook-selectors.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KUEUE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CHECKER="$SCRIPT_DIR/check-webhook-selectors.py"
VENDORED="$KUEUE_DIR/vendor/kueue-v0.10.0.yaml"

require_file() {
    [ -f "$1" ] || {
        printf 'FAIL: required file is missing: %s\n' "$1" >&2
        exit 1
    }
}

require_file "$CHECKER"
require_file "$VENDORED"

python3 "$CHECKER" "$VENDORED"

expect_rejected() {
    local fixture=$1 expected=$2 output status
    fixture="$SCRIPT_DIR/fixtures/$fixture"
    require_file "$fixture"
    set +e
    output=$(python3 "$CHECKER" "$fixture" 2>&1)
    status=$?
    set -e
    if [ "$status" -eq 0 ]; then
        printf 'FAIL: malformed fixture unexpectedly passed: %s\n' "$fixture" >&2
        exit 1
    fi
    if ! grep -Fq -- "$expected" <<<"$output"; then
        printf 'FAIL: %s was rejected for the wrong reason:\n%s\n' "$fixture" "$output" >&2
        exit 1
    fi
    printf 'PASS: checker rejected %s\n' "${fixture#$KUEUE_DIR/}"
}

expect_rejected missing-namespace-selector.yaml 'djinn.io/kueue-managed'
expect_rejected missing-object-selector.yaml 'djinn.io/kueue-build-object'
printf 'PASS: Kueue webhook selector contract completed\n'
