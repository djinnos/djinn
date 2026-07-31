#!/bin/sh
# Reachability guard for proposal 3i92's Pod-resize stack (0ppk-1b, AC2).
#
# WHY THIS EXISTS
#
# On main before 0ppk-1b, every symbol named below had EXACTLY ZERO production
# callers. `BuildPodPermitRepository` created no permit row in production.
# `TaskRunResizeBootstrap::bootstrap` was never called. `DispatchGate::admit` was
# never called, so its unadmitted-dispatch counter sat at zero for the wrong
# reason: not because the gate was holding, but because there was no dispatch
# path to hold. All of it was merged, reviewed, and green.
#
# That is not a hypothetical. This epic's neighbourhood shipped SEVEN slices in
# one day that were correct in isolation and inert in production — a projection
# with no reader, a launcher path unreachable from its own binary, a trait
# override nothing composed, a chart that rendered fine and admitted nothing.
# Every one of them had a passing test suite, because every one of those suites
# constructed the type under test and called its methods.
#
# The compiler cannot catch this: an uncalled `pub` method is not an error, and
# `#[cfg(test)]` callers keep dead-code lints quiet. So the assertion has to be
# made at the granularity the failure actually occurs at — "does anything
# outside a test call this?" — and that is text.
#
# WHAT IS CHECKED
#
#   1. Every guarded symbol has at least one caller in PRODUCTION source: not
#      under a `tests/` directory, not in a `*_tests.rs` file, not in a test
#      helper module, not after the file's `#[cfg(test)]` marker, and not in a
#      checked-out agent worktree.
#   2. Every composition anchor is present. A production caller that lives
#      inside an object nobody constructs is still zero reachability — that is
#      the trait-override failure verbatim — so the guard separately pins the
#      composition root that builds the bridge and threads it into the agent
#      context, and the dispatch seam that acquires the permit.
#   3. Every file the guard scans for anchors exists. A guard whose subject was
#      renamed must fail loudly rather than pass vacuously.
#
# Usage:
#   ./scripts/check-resize-reachability.sh
#   GUARD_ROOT=/path/to/fixture ./scripts/check-resize-reachability.sh
#     (self-test hook; see scripts/test-check-resize-reachability.sh)

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)
ROOT=${GUARD_ROOT:-$REPO_ROOT}

cd "$ROOT"

# symbol|extended-regex for a call site|file must also mention this token
#
# `0ppk-3` added the last two. Before it, `list_nonterminal_resize` had ZERO
# callers of ANY kind outside one repository test — the read the whole external
# reconciler exists to perform, merged and unreachable — and
# `task_run_resize_reconcile::spawn` is the seam that makes a dead worker's
# stranded Pod somebody's problem at all. Both are exactly the shape this guard
# exists to catch.
GUARDED_SYMBOLS="
TaskRunResizeBootstrap::bootstrap|\.bootstrap\(|TaskRunResizeBootstrap
DispatchGate::admit|\.admit\(|DispatchGate
BuildPodPermitRepository::acquire|\.acquire\(|BuildPodPermitRepository
BuildPodPermitRepository::capture_resize_identity|\.capture_resize_identity\(|BuildPodPermitRepository
BuildPodPermitRepository::list_nonterminal_resize|\.list_nonterminal_resize\(|BuildPodPermitRepository
task_run_resize_reconcile::spawn|task_run_resize_reconcile::spawn\(|become_leader
"

# file|extended-regex|why this anchor exists
COMPOSITION_ANCHORS="
server/src/server/state/mod.rs|TaskRunResizeAdmissionBridge::from_env\(|the composition root must actually build the bridge
server/src/server/state/mod.rs|resize_admission: Some\(|the bridge must be threaded into every AgentContext the slot pool dispatches through
server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs|acquire_build_pod_permit\(app_state, spec\)|the dispatch seam must acquire the durable permit before the Job is created
server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs|bind_build_pod_permit_job_uid\(|the dispatch seam must bind the Job UID the runtime just created
server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs|admit_task_run_dispatch\(|the dispatch seam must gate stdio attach on the birth downsize
server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs|record_dispatch_started\(|the dispatch site must report itself so the gate's absence is observable
server/src/server/state/mod.rs|task_run_resize_reconcile::spawn\(self\.clone\(\)\)|the resize reconciler must be armed from become_leader, or a worker death strands its Pod forever
"

status=0

# Emit `path:line:text` for every PRODUCTION line of every Rust source file.
#
# `#[cfg(test)]` truncation is per-file and deliberately unconditional: the
# repository convention is one test module at the bottom of a file (or an
# adjacent `*_tests.rs`), so everything from that marker onwards is test code.
# Erring toward truncating too much makes this guard fail closed — a real
# production call site hidden below a `#[cfg(test)]` marker reads as "no caller"
# and the guard complains, which is the safe direction for a guard whose entire
# purpose is to refuse to believe that something is reachable.
production_lines() {
    find server -name '*.rs' -type f \
        -not -path '*/target/*' \
        -not -path '*/tests/*' \
        -not -path '*/.worktrees/*' \
        -not -name '*_tests.rs' \
        -not -name 'test_helpers.rs' \
        -not -name 'test_support.rs' \
        -not -name 'test_runtime.rs' \
        -print0 |
        xargs -0 -r awk '
            FNR == 1 { skip = 0 }
            /^[[:space:]]*#\[cfg\(test\)\][[:space:]]*$/ { skip = 1 }
            skip == 0 { print FILENAME ":" FNR ":" $0 }
        '
}

PRODUCTION=$(production_lines)

if [ -z "$PRODUCTION" ]; then
    echo "FAIL: found no production Rust sources under server/." >&2
    echo "      The reachability guard has no subject and would pass vacuously." >&2
    exit 1
fi

printf '%s\n' "$GUARDED_SYMBOLS" | while IFS='|' read -r symbol pattern owner; do
    [ -n "$symbol" ] || continue
    hits=$(printf '%s\n' "$PRODUCTION" | grep -E "$pattern" || true)
    # Restrict to files that also name the owning type, so an unrelated
    # `.acquire(` on some other repository cannot satisfy the check.
    matched=""
    for file in $(printf '%s\n' "$hits" | cut -d: -f1 | sort -u); do
        [ -n "$file" ] || continue
        if grep -q -- "$owner" "$file" 2>/dev/null; then
            matched="$matched $file"
        fi
    done
    if [ -z "$matched" ]; then
        echo "FAIL: $symbol has ZERO production callers." >&2
        echo "      Searched every .rs file under server/ outside tests/, outside" >&2
        echo "      *_tests.rs, outside test helper modules, and above each file's" >&2
        echo "      #[cfg(test)] marker." >&2
        echo "      This is the exact state main was in before 0ppk-1b: the whole" >&2
        echo "      resize stack merged, green, and unable to fire in production." >&2
        echo "      If the call site legitimately moved, update $0." >&2
        exit 1
    fi
    printf 'ok: %s is called from:%s\n' "$symbol" "$matched"
done || status=1

printf '%s\n' "$COMPOSITION_ANCHORS" | while IFS='|' read -r file pattern why; do
    [ -n "$file" ] || continue
    if [ ! -f "$file" ]; then
        echo "FAIL: composition anchor file is missing: $file" >&2
        echo "      ($why)" >&2
        exit 1
    fi
    if ! grep -E -q -- "$pattern" "$file"; then
        echo "FAIL: $file no longer matches /$pattern/." >&2
        echo "      Why this is guarded: $why." >&2
        echo "      A production caller inside an object nobody composes is still" >&2
        echo "      zero reachability." >&2
        exit 1
    fi
    printf 'ok: %s anchors /%s/\n' "$file" "$pattern"
done || status=1

if [ "$status" -ne 0 ]; then
    echo "check-resize-reachability: FAILED" >&2
    exit 1
fi

echo "check-resize-reachability: OK"
