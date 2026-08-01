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
# `0ppk-3` added `list_nonterminal_resize` and `task_run_resize_reconcile::spawn`.
# Before it, `list_nonterminal_resize` had ZERO callers of ANY kind outside one
# repository test — the read the whole external reconciler exists to perform,
# merged and unreachable — and `task_run_resize_reconcile::spawn` is the seam
# that makes a dead worker's stranded Pod somebody's problem at all. Both are
# exactly the shape this guard exists to catch.
#
# `ResizeRollout::production` is the last entry, and it was in the same state:
# the staged activation this whole proposal is FOR, composed correctly, tested
# thoroughly, and callable by nobody. Its only reference outside its own module
# was a test asserting the constructor existed. The composition anchors below
# pin the two halves that make it reachable — the driver that calls it and the
# binary that calls the driver — because a production caller inside a function
# no `main` reaches is still zero reachability.
GUARDED_SYMBOLS="
TaskRunResizeBootstrap::bootstrap|\.bootstrap\(|TaskRunResizeBootstrap
DispatchGate::admit|\.admit\(|DispatchGate
BuildPodPermitRepository::acquire|\.acquire\(|BuildPodPermitRepository
BuildPodPermitRepository::capture_resize_identity|\.capture_resize_identity\(|BuildPodPermitRepository
BuildPodPermitRepository::list_nonterminal_resize|\.list_nonterminal_resize\(|BuildPodPermitRepository
task_run_resize_reconcile::spawn|task_run_resize_reconcile::spawn\(|become_leader
ResizeRollout::production|ResizeRollout::production\(|ResizeRollout
BuildLeaseRepository::list_nonterminal_with_settlement|\.list_nonterminal_with_settlement\(|BuildLeaseRepository
BuildLeaseRepository::list_nonterminal|\.list_nonterminal\(|BuildLeaseRepository
BuildLeaseRepository::clear_for_operator|\.clear_for_operator\(|BuildLeaseRepository
BuildLeaseService::expire_deadlines|\.expire_deadlines\(\)|build_lease
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
server/src/bin/authority_cutover.rs|let report = run\(|the operator binary must call the cutover driver, or ResizeRollout::production sits inside a function no main reaches
server/Cargo.toml|^name = .authority-cutover.$|the operator entry point must be a declared binary target; a src/bin file cargo does not build is not an entry point
server/src/authority_cutover.rs|rollout\.activate\(&plan\)|the driver must run the forward sequence through ResizeRollout, not through set_mode
server/src/authority_cutover.rs|rollout\.rollback\(&plan\)|the driver must run the reverse sequence through ResizeRollout, not through set_mode
server/src/task_run_resize_rollout.rs|self\.clear_preflight\(LauncherAuthorityProtocol::ResizeV2\)|the forward cutover must run the real preflight before the flip
server/src/task_run_resize_rollout.rs|self\.clear_preflight\(LauncherAuthorityProtocol::LeafV1\)|the reverse cutover must run the real preflight before the flip
server/src/server/state/mod.rs|BuildLeaseReclaimer::new\(|the build-lease reclaimer must be constructed at the composition root
server/src/server/state/mod.rs|reclaimer\.reclaim\(\)\.await|the reclaimer must be DRIVEN on the periodic tick, not merely constructed; a startup-only reaper never sees a worker that dies at minute 40
server/src/server/state/mod.rs|cap_refresh_lease\.expire_deadlines\(\)\.await|queue and launch deadlines must be swept periodically -- expire_deadlines had ZERO production callers, which is why a queued build lease was immortal whenever no other lease was being drained
server/src/admin.rs|AdminCommand::BuildLease|the operator surface preflight.sh promises must be dispatched from run_admin_command, or its refusal message names a remedy that does not exist
"

status=0

# Emit `path:line:text` for every PRODUCTION line of every Rust source file.
#
# TEST CODE IS TRACKED STRUCTURALLY, NOT BY FIRST MARKER.
#
# This used to truncate the whole file from the first `#[cfg(test)]` line
# onwards, on the theory that the repository convention is one test module at
# the bottom of a file. That theory is wrong twice over, and `0ppk-3` hit both:
#
#   1. `#[cfg(test)]` is an ordinary item attribute. `server/src/server/state/
#      mod.rs` carries one at line 342 — on a STRUCT FIELD inside `Inner`, 1800
#      lines above `become_leader` — so first-marker truncation hid every
#      production call site in the composition root. The guard reported
#      "task_run_resize_reconcile::spawn has ZERO production callers" in the
#      same run in which its own anchor check FOUND that exact call, in that
#      exact file. A guard that contradicts itself teaches people to delete it.
#   2. A `#[cfg(test)] mod tests { .. }` is not always last. Production code
#      after one is still production code.
#
# So the tracker counts braces, exactly as `scripts/check-test-global-metrics.sh`
# does for the same reason (PR #2867 found its health-probe reader sitting after
# a `#[cfg(test)]` block in its own file). A test attribute arms the tracker; if
# the item it attaches to opens a block, everything to the matching close brace
# is test code and nothing after it is. An attribute on a field, a variant or a
# match arm attaches to no block and suppresses nothing.
#
# Braces are counted with double-quoted literals and `//` comments stripped, so
# neither a brace inside a string nor one in a trailing comment can unbalance
# the tracker.
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
            function strip(line,    i, n, c, out, in_str) {
                n = length(line); i = 1; out = ""; in_str = 0
                while (i <= n) {
                    c = substr(line, i, 1)
                    if (in_str) {
                        if (c == "\\") { i += 2; continue }
                        if (c == "\"") { in_str = 0 }
                        i++
                        continue
                    }
                    if (c == "\"") { in_str = 1; i++; continue }
                    if (c == "/" && substr(line, i + 1, 1) == "/") { break }
                    out = out c
                    i++
                }
                return out
            }
            function braces(s,    i, n, c, d) {
                n = length(s); d = 0
                for (i = 1; i <= n; i++) {
                    c = substr(s, i, 1)
                    if (c == "{") d++
                    else if (c == "}") d--
                }
                return d
            }
            FNR == 1 { depth = 0; armed = 0; waiting = 0 }
            {
                s = strip($0)

                # Inside a test block: consume to the matching close brace.
                if (depth > 0) {
                    depth += braces(s)
                    if (depth < 0) depth = 0
                    next
                }

                # An item that a test attribute introduced, whose opening brace
                # has not arrived yet (a multi-line `fn` signature).
                if (waiting == 1) {
                    d = braces(s)
                    if (d > 0) { depth = d; waiting = 0; next }
                    if (s ~ /;[ \t]*$/) { waiting = 0 }
                    next
                }

                if (armed == 1) {
                    # Further attributes, or a blank/doc gap, before the item.
                    if (s ~ /^[ \t]*#\[/ || s ~ /^[ \t]*$/) next
                    if (s ~ /^[ \t]*(pub([ \t]*\([^)]*\))?[ \t]+)?(default[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?(extern[ \t]+"[^"]*"[ \t]+)?(fn|mod|impl|struct|enum|trait|union|use|const|static|type|macro_rules!)[ \t!(]/) {
                        armed = 0
                        d = braces(s)
                        # `#[cfg(test)] mod tests;` opens no block HERE — the
                        # module is a separate file, already excluded by name.
                        if (d > 0) { depth = d; next }
                        if (s ~ /;[ \t]*$/) next
                        waiting = 1
                        next
                    }
                    # The attribute was on a field, a variant or a match arm.
                    # It governs that one line and nothing after it.
                    armed = 0
                    next
                }

                if (s ~ /^[ \t]*#\[(cfg\(test\)|test|tokio::test|rstest)/) {
                    armed = 1
                    next
                }

                print FILENAME ":" FNR ":" $0
            }
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
