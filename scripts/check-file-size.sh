#!/bin/sh
# Rust source-file size guard.
#
# Usage:
#   ./scripts/check-file-size.sh
#   MAX_LINES=1200 MAX_BYTES=45000 ./scripts/check-file-size.sh
#
# Checks Rust files under server/crates/ and server/src/ relative to the
# repository root. A file is oversized if it exceeds either threshold:
#   MAX_LINES  maximum line count, default 1500
#   MAX_BYTES  maximum byte count, default 51200
#
# Generated files are skipped defensively: paths containing /generated/ and
# files with a .gen.* suffix are ignored. If a genuinely-large Rust source file
# must exceed the thresholds, add this exact token anywhere in the file:
#   // djinn:allow-oversize
# Oversized files with that marker are reported as allowed and do not fail.

set -eu

MAX_LINES=${MAX_LINES:-1500}
MAX_BYTES=${MAX_BYTES:-51200}

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)

cd "$REPO_ROOT"

find_roots=""
if [ -d server/crates ]; then
    find_roots="$find_roots server/crates"
fi
if [ -d server/src ]; then
    find_roots="$find_roots server/src"
fi

if [ -z "$find_roots" ]; then
    echo "No Rust source roots found (server/crates, server/src)."
    exit 0
fi

# Enumerate once, skip generated files before counting, and let wc count every
# remaining file in bulk. This keeps the guard fast while still producing one
# wc result line per checked source file.
# find_roots contains only fixed, space-free repository paths.
# shellcheck disable=SC2086
find $find_roots \
    -type f \
    -name '*.rs' \
    ! -path '*/generated/*' \
    ! -name '*.gen.*' \
    -exec wc -lc {} + | {
    violations=0
    allowed=0

    while read -r lines bytes file extra; do
        # wc may emit a "total" line when it receives multiple files in one
        # invocation. Multiple find -exec chunks can therefore produce multiple
        # totals; all are aggregate-only and not source files.
        if [ "$file" = total ]; then
            continue
        fi
        if [ -n "${extra:-}" ]; then
            file="$file $extra"
        fi

        if [ "$lines" -gt "$MAX_LINES" ] || [ "$bytes" -gt "$MAX_BYTES" ]; then
            if grep -q 'djinn:allow-oversize' "$file"; then
                printf 'OK    %s  (allowed: djinn:allow-oversize; %s lines, %s bytes)\n' \
                    "$file" "$lines" "$bytes"
                allowed=$((allowed + 1))
            else
                printf 'FAIL  %s  (%s lines, %s bytes)\n' "$file" "$lines" "$bytes"
                violations=$((violations + 1))
            fi
        fi
    done

    if [ "$violations" -gt 0 ]; then
        printf 'Found %s oversized file(s) under MAX_LINES=%s / MAX_BYTES=%s.' \
            "$violations" "$MAX_LINES" "$MAX_BYTES"
        if [ "$allowed" -gt 0 ]; then
            printf ' %s oversized file(s) allowed by marker.' "$allowed"
        fi
        printf '\n'
        exit 1
    fi

    printf 'Found 0 oversized file(s) under MAX_LINES=%s / MAX_BYTES=%s.' \
        "$MAX_LINES" "$MAX_BYTES"
    if [ "$allowed" -gt 0 ]; then
        printf ' %s oversized file(s) allowed by marker.' "$allowed"
    fi
    printf '\n'
    exit 0
}
