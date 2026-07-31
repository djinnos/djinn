#!/bin/sh
# Rust source-file size REPORT.
#
# This script measures. It does not gate. It exits 0 whether or not the tree
# contains oversized files; the only non-zero exits are usage errors (2).
#
# ## Why this stopped being a gate
#
# It landed 2026-06-11 as a hard failure on changed files. Measured 2026-07-30,
# after seven weeks:
#
#   * files carrying `// djinn:allow-oversize`: 0 -> 108
#   * oversized files (by line count):          22 -> 79
#   * files split in response to the gate:       0
#
# 101 of the 108 marker-carrying files are genuinely oversized, and NO oversized
# file lacks a marker. Every file that ever tripped the gate got a marker; none
# got restructured. That is the whole distribution, not a sample.
#
# The mechanism is an incentive gap, not a discipline problem: complying means
# restructuring a module (hours, a behaviour-bearing refactor, review risk)
# while evading means adding one comment line (seconds, zero risk). At ~100x
# cost asymmetry evasion always wins. Worse, a per-file line limit is
# satisfiable by cutting a file at ANY point, and the `_partN` fragments that
# produced hid 20.7k lines from rust-analyzer, rustfmt and three CI guards.
#
# On 2026-07-30 the gate blocked two correctness fixes on MAX_BYTES — PR #2815
# (141 bytes of headroom, tipped over by its own explanatory comment) and PR
# #2839 (267 bytes, tipped over by the coverage fix itself). Both were resolved
# by adding a marker. That is the gate taxing correctness work and collecting
# nothing.
#
# So: keep the measurement, drop the veto. Pressure toward workable file sizes
# now lives at authorship time, in the agent's edit/write/apply_patch tool
# results, where there is nothing to satisfy and therefore nothing to game.
#
# ## Why the marker is still parsed
#
# Nothing fails, so the marker no longer grants permission — but it still
# carries information: it is the author's on-the-record statement that this
# file's size is deliberate. The report uses it to separate `acknowledged` from
# `unacknowledged` rows, which is the only axis on which the ranking is
# actionable. The existing 108 markers are deliberately left in place; removing
# them would be 108 files of churn for zero behavioural change.
#
# ## Ranking
#
# Rows are ordered by BYTES descending (lines descending, then path, as
# tiebreaks). Bytes is the binding constraint on whether a reader can load a
# file at all: an agent's `read` returns at most 2000 lines AND at most
# `output_stash::MAX_TOOL_RESULT_CHARS` (30_000) characters, so byte size is
# what decides how many calls it takes to see the file. Line count is reported
# alongside because it is the more legible number to a human.
#
# Usage:
#   ./scripts/check-file-size.sh --all
#   ./scripts/check-file-size.sh
#   printf '%s\n' server/crates/foo/src/lib.rs | ./scripts/check-file-size.sh --files-from-stdin
#   SIZE_GUARD_MODE=all ./scripts/check-file-size.sh
#   SIZE_GUARD_MODE=files-from-stdin ./scripts/check-file-size.sh < changed-files.txt
#
# Reports on Rust files under server/crates/ and server/src/ relative to the
# repository root. Full-tree mode audits every in-scope Rust file. Changed-file
# mode evaluates only supplied paths that are in scope and still exist.
# A file is listed as oversized if it exceeds either report threshold:
#   MAX_LINES  maximum line count, default 1500
#   MAX_BYTES  maximum byte count, default 51200
#
# Generated files are skipped defensively: paths containing /generated/ and
# files with a .gen.* suffix are ignored.
#
# When $GITHUB_STEP_SUMMARY is set the ranked table is also appended there as
# markdown, and the worst offenders are emitted as `::notice` annotations
# (capped at ANNOTATION_LIMIT, because GitHub renders at most 10 per step).

set -eu

MAX_LINES=${MAX_LINES:-1500}
MAX_BYTES=${MAX_BYTES:-51200}
ALLOW_MARKER='djinn:allow-oversize'
MODE=${SIZE_GUARD_MODE:-all}
ANNOTATION_LIMIT=${ANNOTATION_LIMIT:-10}

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)

cd "$REPO_ROOT"

usage() {
    cat <<EOF
Usage: $0 [--all|--files-from-stdin]

Reports Rust source-file sizes. Never fails on an oversized file.

Modes:
  --all               Audit all in-scope Rust files (default; also SIZE_GUARD_MODE=all).
  --files-from-stdin  Read changed file paths from stdin and audit only in-scope Rust files
                      that still exist (also SIZE_GUARD_MODE=files-from-stdin).

Environment:
  MAX_LINES           Line count above which a file is listed (default: 1500).
  MAX_BYTES           Byte count above which a file is listed (default: 51200).
  ANNOTATION_LIMIT    Max ::notice annotations emitted (default: 10).
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

# Emit the ranked report from a tab-separated record file whose columns are:
#   bytes<TAB>lines<TAB>status<TAB>path
#
# `checked` is the number of in-scope files measured. The function always
# returns success; there is no failure path.
emit_report() {
    records=$1
    checked=$2

    oversized=$(wc -l < "$records" | tr -d '[:space:]')

    if [ "$oversized" -eq 0 ]; then
        printf 'Rust source-file size report (report-only; nothing here fails the build).\n'
        printf 'Report thresholds: MAX_LINES=%s MAX_BYTES=%s.\n' "$MAX_LINES" "$MAX_BYTES"
        printf 'Found 0 oversized file(s). Checked %s Rust source file(s).\n' "$checked"
        return 0
    fi

    # Rank: bytes desc, then lines desc, then path asc for a stable order.
    sorted="$records.sorted"
    sort -t "$(printf '\t')" -k1,1nr -k2,2nr -k4,4 "$records" > "$sorted"

    unacknowledged=$(cut -f3 "$sorted" | grep -c '^unacknowledged$' || true)
    acknowledged=$((oversized - unacknowledged))

    printf 'Rust source-file size report (report-only; nothing here fails the build).\n'
    printf 'Report thresholds: MAX_LINES=%s MAX_BYTES=%s.\n' "$MAX_LINES" "$MAX_BYTES"
    printf 'Ranked worst-first by bytes (a reader loads bytes, not lines).\n\n'
    printf '  RANK      BYTES    LINES  STATUS          PATH\n'

    rank=0
    while IFS="$(printf '\t')" read -r bytes lines status path; do
        rank=$((rank + 1))
        printf '  %4d  %9s  %7s  %-14s  %s\n' "$rank" "$bytes" "$lines" "$status" "$path"
    done < "$sorted"

    printf '\nFound %s oversized file(s) (%s unacknowledged, %s acknowledged by %s). ' \
        "$oversized" "$unacknowledged" "$acknowledged" "$ALLOW_MARKER"
    printf 'Checked %s Rust source file(s).\n' "$checked"

    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        {
            printf '### Rust source-file size report\n\n'
            printf 'Report only — this step cannot fail the build. '
            printf '%s oversized (%s unacknowledged, %s acknowledged) of %s checked, ' \
                "$oversized" "$unacknowledged" "$acknowledged" "$checked"
            printf 'thresholds `MAX_LINES=%s` / `MAX_BYTES=%s`.\n\n' "$MAX_LINES" "$MAX_BYTES"
            printf '| # | bytes | lines | status | path |\n'
            printf '| --: | --: | --: | --- | --- |\n'
            rank=0
            while IFS="$(printf '\t')" read -r bytes lines status path; do
                rank=$((rank + 1))
                printf '| %d | %s | %s | %s | `%s` |\n' \
                    "$rank" "$bytes" "$lines" "$status" "$path"
            done < "$sorted"
            printf '\n'
        } >> "$GITHUB_STEP_SUMMARY"
    fi

    # Annotations, worst first. GitHub renders at most 10 per step, so asking
    # for more just discards the tail silently — cap it explicitly instead.
    rank=0
    while IFS="$(printf '\t')" read -r bytes lines status path; do
        rank=$((rank + 1))
        if [ "$rank" -gt "$ANNOTATION_LIMIT" ]; then
            break
        fi
        printf '::notice file=%s,line=1,title=Large Rust source (report only)::%s is %s bytes / %s lines (%s). Report only — nothing is blocked.\n' \
            "$path" "$path" "$bytes" "$lines" "$status"
    done < "$sorted"

    rm -f "$sorted"
    return 0
}

check_files() {
    checked=0

    records=$(mktemp "${TMPDIR:-/tmp}/djinn-size-report.XXXXXX")
    # The report is best-effort measurement; never leave scratch behind, and
    # never let a cleanup failure become the step's exit status.
    trap 'rm -f "$records" "$records.sorted" 2>/dev/null || true' EXIT INT TERM

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
                status=acknowledged
            else
                status=unacknowledged
            fi
            printf '%s\t%s\t%s\t%s\n' "$bytes" "$lines" "$status" "$file" >> "$records"
        fi
    done

    emit_report "$records" "$checked"
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
