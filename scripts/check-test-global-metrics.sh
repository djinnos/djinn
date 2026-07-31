#!/bin/sh
# Guard: test code must not read the process-global metrics registry.
#
# WHY THIS EXISTS
#
# `djinn_telemetry::render()` renders ONE registry, installed process-wide by
# `djinn_telemetry::init()`. `cargo test` runs every test in a binary as a
# thread of a single process, so a test that renders that registry is reading
# the whole binary's cumulative state. Two failure shapes follow, and this repo
# has now hit both six times:
#
#   * gauges  — `set()` is last-writer-wins, so an absolute assertion is an
#               assertion about whichever sibling test wrote most recently;
#   * counters — a before/after delta narrows the window but does not close it,
#               because a concurrent writer can land between the two renders.
#
# Both surface as low-rate flakes in a DIFFERENT test each run, which is why
# they cost six separate investigations (#2820, #2824, #2851, and the three
# residuals fixed alongside this script) instead of one.
#
# The fix is mechanical and already exists: `djinn_telemetry::render_isolated`
# for a synchronous body, `djinn_telemetry::IsolatedRecorder` + `scope()` for an
# async one. Both give the test a registry no other test can reach, so exact
# assertions become deterministic without serializing anything.
#
# WHY IT GATES ONLY ADDED LINES
#
# Measured on this tree: `cargo nextest run` gives every test its own process,
# so none of these collisions is observable in the merge queue — the workspace
# lane cannot fail on them. Reproduced on the pre-fix djinn-db tests:
#
#   cargo test    -p djinn-db --lib parse_archive_window_days .... 21/40 FAILED
#   cargo nextest run -p djinn-db --lib -E 'test(parse_...)' ....   0/15 failed
#
# They bite the developer loop instead (`make test` is `cargo test -p djinn-db`).
# There are ~150 pre-existing `render()` reads across ~30 files, many inside
# tests that hand work to `tokio::spawn` — where the conversion needs real
# thought, not a mechanical edit. A whole-tree gate would therefore demand a
# large migration whose payoff CI cannot even observe.
#
# THERE IS DELIBERATELY NO OPT-OUT MARKER, for the reason recorded at the top
# of check-file-size.sh: that guard's `djinn:allow-oversize` escape went 0 ->
# 108 markers while oversized files went 22 -> 79, because evading cost one
# comment line and complying cost a restructure. Restricting this rule to added
# lines keeps the opposite ratio — writing `render_isolated` instead of
# `render` is the same amount of typing — so no escape hatch is needed.
#
# WHAT IS EXEMPT, BY PATTERN
#
#   1. `server/crates/djinn-telemetry/` — it owns the singleton and must be
#      able to test `init()`/`render()` themselves.
#   2. Production code. A `render()` outside a `#[cfg(test)]` block and outside
#      a `#[test]`-attributed function is the metrics endpoint or a health
#      probe doing exactly what the global registry is for.
#
# Usage:
#   ./scripts/check-test-global-metrics.sh <base-sha>   # added lines vs base
#   ./scripts/check-test-global-metrics.sh --all        # whole tree (report)
#
# Exit codes: 0 clean, 1 violations found, 2 usage/environment error.

set -eu

# The shared scanner is resolved from THIS SCRIPT's location, deliberately
# unlike REPO_ROOT below: the tree being scanned may be a throwaway fixture
# repo, but the scanner is always the one shipped next to this guard.
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

# Resolve the tree to scan from the CALLER's working directory rather than from
# this script's own location. CI invokes it from the repo root either way, and
# deriving it this way is what lets the self-test drive the real guard against a
# throwaway fixture repo instead of a mode that only the test exercises.
if ! REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null); then
    echo "::error::check-test-global-metrics: not a git repository." >&2
    exit 2
fi
cd "$REPO_ROOT"

case "${1:-}" in
-h | --help)
    sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
--all) MODE=all ;;
"")
    echo "check-test-global-metrics: a base SHA is required (or --all)." >&2
    echo "usage: $0 <base-sha> | --all" >&2
    exit 2
    ;;
*)
    MODE=diff
    BASE=$1
    ;;
esac

# Emit "lineno" for every line of $1 that is inside test code AND reads the
# process-global registry.
#
# "Inside test code" is tracked structurally rather than by filename: a
# `#[cfg(test)]`-attributed `mod ... { }` block, or a `#[test]` /
# `#[tokio::test]` / `#[rstest]`-attributed item, up to its matching close
# brace. Braces are counted on a line with double-quoted string literals and
# `//` comments stripped, so neither a brace in a string nor one in a trailing
# comment can unbalance the tracker. This is what keeps the two legitimate
# production readers (the /metrics HTTP handler and the coordinator health
# probe) out of scope even though both live in files that also contain tests.
#
# A whole file counts as test code when its path says so: an integration-test
# directory, or a module a parent declares as `#[cfg(test)] mod x;`. In that
# second case the attribute lives in the PARENT file, so the structural tracker
# above sees nothing — and a bare helper `fn` in such a module (no `#[test]`
# attribute of its own) would otherwise slip through. That is not hypothetical:
# `extension/tests/jit_trace_tests.rs` reads the global registry from exactly
# such a helper.
whole_file_is_test() {
    case "$1" in
    */tests/*) return 0 ;;
    */tests.rs | *_tests.rs | *_test.rs) return 0 ;;
    esac
    return 1
}

# The tracker described above now lives in scripts/lib/rust-source-scan.awk,
# shared with every other Rust source-text guard and self-tested in both
# directions by scripts/test-rust-source-scan.sh.
#
# It moved because the copy that used to sit here broke this guard's own
# documented exemption 2 ("Production code ... is exempt"). An armed
# `#[cfg(test)]` disarmed only on a line ending in `;`, so the attribute on a
# struct FIELD -- which ends in `,` -- left the tracker armed until the next
# line that opened a brace, which is normally the next PRODUCTION function.
# Measured on a fixture of exactly that shape (a `#[cfg(test)]` field followed
# by a plain `pub fn`), a production `render()` was reported as a test read.
# `server/src/server/state/mod.rs` carries that shape from line 342 onward.
#
# The shared tracker disarms on any `;`- or `,`-terminated item and carries
# paren depth, so a multi-line `#[test] async fn foo(\n a: A,\n) {` signature
# still resolves to its block rather than disarming mid-signature.
SCAN_AWK="$SCRIPT_DIR/lib/rust-source-scan.awk"
if [ ! -f "$SCAN_AWK" ]; then
    echo "::error::check-test-global-metrics: missing shared scanner $SCAN_AWK" >&2
    exit 2
fi

scan_test_reads() {
    RS_PATTERN='djinn_telemetry::render[ \t]*\(' \
        awk -f "$SCAN_AWK" -v strings=blank -v scope=test -v force_test="${2:-0}" "$1" |
        cut -d: -f2
}

is_exempt() {
    case "$1" in
    server/crates/djinn-telemetry/*) return 0 ;;
    *) return 1 ;;
    esac
}

if [ "$MODE" = all ]; then
    FILES=$(git ls-files -- '*.rs')
else
    if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
        echo "::error::check-test-global-metrics: base SHA '$BASE' is not resolvable." >&2
        exit 2
    fi
    FILES=$(git diff --name-only --diff-filter=AMR "$BASE...HEAD" -- '*.rs')
fi

ADDED_TMP=$(mktemp)
trap 'rm -f "$ADDED_TMP"' EXIT INT TERM

violations=""
for f in $FILES; do
    [ -n "$f" ] || continue
    [ -f "$f" ] || continue
    is_exempt "$f" && continue

    if whole_file_is_test "$f"; then
        hits=$(scan_test_reads "$f" 1)
    else
        hits=$(scan_test_reads "$f" 0)
    fi
    [ -n "$hits" ] || continue

    if [ "$MODE" = diff ]; then
        # Added line numbers in the post-image, from -U0 hunk headers.
        added=$(git diff --unified=0 "$BASE...HEAD" -- "$f" |
            awk '/^@@/ {
                    match($0, /\+[0-9]+(,[0-9]+)?/)
                    spec = substr($0, RSTART + 1, RLENGTH - 1)
                    split(spec, p, ",")
                    count = (p[2] == "") ? 1 : p[2]
                    for (i = 0; i < count; i++) print p[1] + i
                 }')
        printf '%s\n' "$added" >"$ADDED_TMP"
        hits=$(printf '%s\n' "$hits" | grep -Fxf "$ADDED_TMP" || true)
        [ -n "$hits" ] || continue
    fi

    for n in $hits; do
        violations="${violations}  ${f}:${n}: $(sed -n "${n}p" "$f" | sed 's/^[ \t]*//')
"
    done
done

if [ -n "$violations" ]; then
    echo "::error::Test code must not read the process-global metrics registry. \`djinn_telemetry::render()\` returns the whole test binary's cumulative state, so a sibling test running concurrently decides what you assert on. Use \`djinn_telemetry::render_isolated\` (sync) or \`djinn_telemetry::IsolatedRecorder\` + \`scope()\` (async)." >&2
    echo "" >&2
    printf '%s' "$violations" >&2
    echo "" >&2
    echo "How to fix:" >&2
    echo "  sync   let (_, rendered) = djinn_telemetry::render_isolated(|| emit());" >&2
    echo "  async  let recorder = djinn_telemetry::IsolatedRecorder::new();" >&2
    echo "         let guard = recorder.scope();   // hold across the region" >&2
    echo "         ... ; let rendered = recorder.render();" >&2
    echo "" >&2
    echo "Both give the test a registry no sibling can reach, so before/after" >&2
    echo "deltas become unnecessary and absolute assertions become exact." >&2
    echo "There is no opt-out marker, by design." >&2
    exit 1
fi

if [ "$MODE" = all ]; then
    echo "OK: no test-code reads of the process-global metrics registry (whole tree)."
else
    echo "OK: no newly-added test-code reads of the process-global metrics registry."
fi
