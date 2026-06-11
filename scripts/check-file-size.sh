#!/bin/sh
# Rust source-file size guard.
#
# Usage:
#   ./scripts/check-file-size.sh --all
#   ./scripts/check-file-size.sh
#   printf '%s\n' server/crates/foo/src/lib.rs | ./scripts/check-file-size.sh --files-from-stdin
#   SIZE_GUARD_MODE=all ./scripts/check-file-size.sh
#   SIZE_GUARD_MODE=files-from-stdin ./scripts/check-file-size.sh < changed-files.txt
#
# Checks Rust files under server/crates/ and server/src/ relative to the
# repository root. Full-tree mode audits every in-scope Rust file. Changed-file
# mode evaluates only supplied paths that are in scope and still exist.
# A file is oversized if it exceeds either threshold:
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
ALLOW_MARKER='djinn:allow-oversize'
MODE=${SIZE_GUARD_MODE:-all}

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)

cd "$REPO_ROOT"

usage() {
    cat <<EOF
Usage: $0 [--all|--files-from-stdin]

Modes:
  --all               Audit all in-scope Rust files (default; also SIZE_GUARD_MODE=all).
  --files-from-stdin  Read changed file paths from stdin and audit only in-scope Rust files
                      that still exist (also SIZE_GUARD_MODE=files-from-stdin).

Environment:
  MAX_LINES           Maximum line count before a file is oversized (default: 1500).
  MAX_BYTES           Maximum byte count before a file is oversized (default: 51200).
EOF
}

if [ "$#" -gt 1 ]; then
    usage >&2
    exit 2
fi

if [ "$#" -eq 1 ]; then
    case "$1" in
        --all)
            MODE=all
            ;;
        --files-from-stdin)
            MODE=files-from-stdin
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
fi

case "$MODE" in
    all|files-from-stdin)
        ;;
    *)
        printf 'Unknown SIZE_GUARD_MODE: %s\n' "$MODE" >&2
        usage >&2
        exit 2
        ;;
esac

is_in_scope_rs_file() {
    path=$1

    case "$path" in
        server/crates/*.rs|server/src/*.rs)
            ;;
        *)
            return 1
            ;;
    esac

    case "$path" in
        */generated/*|*.gen.*)
            return 1
            ;;
    esac

    return 0
}

check_files() {
    checked=0
    violations=0
    allowed=0

    while IFS= read -r file || [ -n "$file" ]; do
        [ -n "$file" ] || continue
        case "$file" in
            ./*)
                file=${file#./}
                ;;
        esac
        is_in_scope_rs_file "$file" || continue
        [ -f "$file" ] || continue

        checked=$((checked + 1))
        lines=$(wc -l < "$file" | tr -d '[:space:]')
        bytes=$(wc -c < "$file" | tr -d '[:space:]')

        if [ "$lines" -gt "$MAX_LINES" ] || [ "$bytes" -gt "$MAX_BYTES" ]; then
            if grep -q "$ALLOW_MARKER" "$file"; then
                printf 'OK    %s  (allowed: %s; %s lines, %s bytes)\n' \
                    "$file" "$ALLOW_MARKER" "$lines" "$bytes"
                allowed=$((allowed + 1))
            else
                printf 'FAIL  %s  (%s lines, %s bytes)\n' "$file" "$lines" "$bytes"
                violations=$((violations + 1))
            fi
        fi
    done

    if [ "$checked" -eq 0 ]; then
        printf 'Checked 0 Rust source file(s); found 0 oversized file(s) under MAX_LINES=%s / MAX_BYTES=%s.\n' \
            "$MAX_LINES" "$MAX_BYTES"
        exit 0
    fi

    if [ "$violations" -gt 0 ]; then
        printf 'Found %s oversized file(s) under MAX_LINES=%s / MAX_BYTES=%s. Checked %s Rust source file(s).' \
            "$violations" "$MAX_LINES" "$MAX_BYTES" "$checked"
        if [ "$allowed" -gt 0 ]; then
            printf ' %s oversized file(s) allowed by marker.' "$allowed"
        fi
        printf '\n'
        exit 1
    fi

    printf 'Found 0 oversized file(s) under MAX_LINES=%s / MAX_BYTES=%s. Checked %s Rust source file(s).' \
        "$MAX_LINES" "$MAX_BYTES" "$checked"
    if [ "$allowed" -gt 0 ]; then
        printf ' %s oversized file(s) allowed by marker.' "$allowed"
    fi
    printf '\n'
    exit 0
}

run_all() {
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

    # find_roots contains only fixed, space-free repository paths.
    # shellcheck disable=SC2086
    find $find_roots \
        -type f \
        -name '*.rs' \
        ! -path '*/generated/*' \
        ! -name '*.gen.*' \
        -print | check_files
}

case "$MODE" in
    all)
        run_all
        ;;
    files-from-stdin)
        check_files
        ;;
esac
