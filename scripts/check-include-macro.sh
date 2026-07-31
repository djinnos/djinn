#!/bin/sh
# `include!` guard for hand-written Rust source.
#
# WHY THIS EXISTS
#
# `rust-analyzer scip` emits NO document for a file pulled in with `include!`.
# The fragment's text is expanded into the including file's token stream, but
# the indexer records occurrences against the file rust-analyzer parsed as a
# module — so the fragment's own path never appears in the index and the
# symbols defined inside it are not findable anywhere.
#
# Measured against this repo's own 108 MB SCIP artifact (v1/bb/bb57ab16..):
#
#   .rs paths in the index ............ 1345
#   .inc paths in the index ...........    0
#   graph_tools/df6s_pagination.rs .... PRESENT  (ordinary `mod`-declared file)
#   graph_tools/tests.rs .............. PRESENT  (but only its 16-line shim)
#   graph_tools/tests_part1.inc ....... ABSENT
#   link_proposal_to_project .......... ABSENT   (defined inside a fragment)
#
# ~20.7k lines of this repo were invisible to the code graph that agents
# navigate with. Note that `df6s_pagination.rs` was added the same week and IS
# indexed, so this is not indexing staleness: a large `mod`-declared file is
# fully indexed, while an `include!`d file of any size is not indexed at all.
# Several `*.rs`-filtered CI guards (file size, raw SQL) were blind to the same
# content for the same reason.
#
# The fix at every call site is a three-line diff:
#
#   include!("helpers.rs");   ->   mod helpers;      (+ `use super::*;` in the
#                                                     fragment, which was
#                                                     textually inside its
#                                                     parent before)
#
# THERE IS DELIBERATELY NO OPT-OUT MARKER.
#
# The sibling size guard (`check-file-size.sh`) does have one
# (`// djinn:allow-oversize`) and it failed completely: between 2026-06-11 and
# 2026-07-30 the marker count went 0 -> 105 while the number of files over the
# limit went 22 -> 79. Not one file was ever split. It failed because evading
# cost one comment line and complying cost a module restructure — roughly a
# 100x gap.
#
# This rule is built to have the opposite property. Complying is cheaper than
# any evasion, so a marker would only ever be used to skip a cheap fix. If a
# genuine build-script case appears it is exempted BY PATTERN below, never by
# an author-supplied marker: faking that pattern means actually writing a
# build script, which is far more work than typing `mod`.
#
# WHAT IS EXEMPT
#
# Exactly one pattern: the classic build-script include, where the path is
# computed from OUT_DIR and the file therefore does not exist in the source
# tree at all.
#
#   include!(concat!(env!("OUT_DIR"), "/generated.rs"));
#
# Such a file is generated, so there is nothing for SCIP to have indexed and
# nothing an agent could have navigated to. As of this guard's introduction the
# repo has ZERO of these; the exemption is defined so the rule stays correct if
# one ever appears, not because anything needs it today.
#
# Usage:
#   ./scripts/check-include-macro.sh                  # scan all tracked *.rs
#   ./scripts/check-include-macro.sh --files-from-stdin < paths
#
# Exit codes: 0 clean, 1 violations found, 2 usage/environment error.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

MODE=all
case "${1:-}" in
"") MODE=all ;;
--files-from-stdin) MODE=stdin ;;
-h | --help)
    sed -n '2,66p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
*)
    echo "check-include-macro: unknown argument '$1'" >&2
    echo "usage: $0 [--files-from-stdin]" >&2
    exit 2
    ;;
esac

if [ "$MODE" = all ]; then
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        echo "::error::check-include-macro: not a git repository; use --files-from-stdin." >&2
        exit 2
    fi
    FILE_LIST=$(git ls-files -- '*.rs')
else
    FILE_LIST=$(cat)
fi

# Emit "path:lineno:text" for every offending `include!`.
#
# The scan is delegated to scripts/lib/rust-source-scan.awk, which is shared
# with every other Rust source-text guard in this directory. It strips Rust
# double-quoted string literals (honouring backslash escapes), `//` line
# comments and `/* */` block comments before looking for the macro, so neither
# a mention in a comment nor a `format!("include!(\"{f}\")")` inside a test can
# trip the guard — while the code IN FRONT of a trailing comment survives, so
# `include!("x.rs"); // legacy` is still reported.
#
# The OUT_DIR exemption is evaluated against the ORIGINAL line (RS_EXEMPT),
# because blanking string literals would erase the "OUT_DIR" token it keys on.
SCAN_AWK="$SCRIPT_DIR/lib/rust-source-scan.awk"
if [ ! -f "$SCAN_AWK" ]; then
    echo "::error::check-include-macro: missing shared scanner $SCAN_AWK" >&2
    exit 2
fi

scan() {
    RS_PATTERN='include![ \t]*\(' \
        RS_EXEMPT='include![ \t]*\([ \t]*concat!.*env![ \t]*\([ \t]*"OUT_DIR"[ \t]*\)' \
        awk -f "$SCAN_AWK" -v strings=blank "$@"
}

violations=$(
    printf '%s\n' "$FILE_LIST" | while IFS= read -r f; do
        [ -n "$f" ] || continue
        [ -f "$f" ] || continue
        scan "$f"
    done
)

if [ -n "$violations" ]; then
    echo "::error::\`include!\` of hand-written source is not permitted. rust-analyzer emits no SCIP document for an included file, so its symbols are unfindable by every agent and by the *.rs-filtered CI guards. Replace each with a module declaration." >&2
    echo "" >&2
    printf '%s\n' "$violations" | while IFS= read -r v; do
        [ -n "$v" ] && echo "  $v" >&2
    done
    echo "" >&2
    echo "How to fix (a ~3-line diff per site):" >&2
    echo "  1. Rename the fragment to <name>.rs and place it where the module tree expects it" >&2
    echo "     (a sibling for a file-module parent, or <parent>/<name>.rs for an inline one)." >&2
    echo "  2. Replace  include!(\"<name>.rs\");  with  mod <name>;  keeping any #[cfg(test)]." >&2
    echo "  3. Add  use super::*;  at the top of the fragment — it used to be textually inside" >&2
    echo "     its parent, so it could reach the parent's private items." >&2
    echo "" >&2
    echo "There is no opt-out marker, by design. The only exemption is the build-script" >&2
    echo "pattern include!(concat!(env!(\"OUT_DIR\"), ...)), where no source file exists to" >&2
    echo "index in the first place." >&2
    exit 1
fi

count=$(printf '%s\n' "$FILE_LIST" | grep -c . || true)
echo "OK: no include! of hand-written source (${count} Rust file(s) scanned)."
