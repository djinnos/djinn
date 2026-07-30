#!/usr/bin/env bash
# Hermetic contract for deploy/kueue/preflight.sh.
#
# No cluster, no credentials, no database. `kubectl` and `psql` are replaced by
# fakes that answer from committed fixture files and log every invocation, so
# both what the preflight ASKS and what it CONCLUDES are assertable.
#
# NON-VACUITY IS THE POINT. Every negative scenario asserts the exact exit code
# of the gate it is supposed to trip plus the diagnostic that gate prints, so a
# fixture cannot "pass" by failing somewhere else. The positive scenarios
# (cutover-clear, cutover-terminal-jobs-only, terminal-leases-only,
# rollback-clear) exist for the other half of the same argument: they prove the
# checker is not simply always red. The read-only assertion is itself proven
# non-vacuous by a mutant that adds a `kubectl label` call and must be caught.
#
# Usage: bash deploy/kueue/tests/preflight.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KUEUE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$KUEUE_DIR/../.." && pwd)"
PREFLIGHT="$KUEUE_DIR/preflight.sh"
RESPONSES="$SCRIPT_DIR/fixtures/preflight"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

# The COMMITTED mode, read from the tree object. `git ls-files -s` reports the
# index, which can carry a mode that never reaches a commit — a sibling task in
# this epic family burned several sessions on exactly that difference.
require_committed_executable() {
    local path=$1 entry
    entry=$(git -C "$REPO_ROOT" ls-tree HEAD "$path")
    [ -n "$entry" ] || fail "$path is not present in HEAD (nothing to attest a mode for)"
    case "$entry" in
        100755\ *) ;;
        *) fail "$path is not committed executable; git ls-tree HEAD reports: $entry" ;;
    esac
}

require_committed_executable deploy/kueue/preflight.sh
require_committed_executable deploy/kueue/tests/preflight.sh

[ -d "$RESPONSES" ] || fail "preflight fixtures are missing: $RESPONSES"

cat >"$WORK/kubectl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'kubectl' >>"$FAKE_LOG"
printf ' %q' "$@" >>"$FAKE_LOG"
printf '\n' >>"$FAKE_LOG"
args=" $* "
scenario_dir="$FAKE_RESPONSES/$FAKE_SCENARIO"
if [[ "$args" == *' get workloads.kueue.x-k8s.io '* ]] && [ "$FAKE_SCENARIO" = api-error ]; then
    printf 'error: the server could not find the requested resource (get workloads.kueue.x-k8s.io)\n' >&2
    exit 1
fi
if [[ "$args" == *' get runtimeclasses.node.k8s.io '* ]]; then
    cat "$scenario_dir/runtimeclasses.txt"
elif [[ "$args" == *' get namespace '* ]]; then
    cat "$scenario_dir/namespace-label.txt"
elif [[ "$args" == *' get jobs '* && "$args" == *'djinn.app/component=task-run-worker'* ]]; then
    cat "$scenario_dir/taskrun-jobs.txt"
elif [[ "$args" == *' get jobs '* && "$args" == *'djinn.app/component=graph-warm'* ]]; then
    cat "$scenario_dir/warm-jobs.txt"
elif [[ "$args" == *' get workloads.kueue.x-k8s.io '* ]]; then
    cat "$scenario_dir/workloads.txt"
else
    # An invocation this fake does not model is a drift signal, not a default.
    # Answering it with silence would let a rewritten query read as an empty
    # population and pass the fence.
    printf 'fake kubectl: unmodelled invocation: %s\n' "$args" >&2
    exit 99
fi
EOF
cat >"$WORK/psql" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
# One argument per line, unquoted: the SQL text IS the fence, and %q-escaping
# it would make the assertion below match escaped prose instead of the query.
printf 'psql\n' >>"$FAKE_PSQL_LOG"
for argument in "$@"; do printf '%s\n' "$argument" >>"$FAKE_PSQL_LOG"; done
if [ "$FAKE_SCENARIO" = ledger-unreadable ]; then
    printf 'psql: error: connection to server failed: Connection refused\n' >&2
    exit 2
fi
cat "$FAKE_RESPONSES/$FAKE_SCENARIO/leases.txt"
EOF
chmod +x "$WORK/kubectl" "$WORK/psql"

# Runs one scenario against a script (the real preflight unless overridden by
# $SCRIPT_UNDER_TEST) and asserts the EXACT exit code plus every diagnostic.
SCRIPT_UNDER_TEST="$PREFLIGHT"
CASE_LOG=''
CASE_PSQL_LOG=''
CASE_OUTPUT=''
run_case() {
    local scenario=$1 mode=$2 expected_exit=$3
    shift 3
    local tag="$scenario-$mode"
    CASE_OUTPUT="$WORK/$tag.out"
    CASE_LOG="$WORK/$tag.log"
    CASE_PSQL_LOG="$WORK/$tag.psql.log"
    : >"$CASE_LOG"
    : >"$CASE_PSQL_LOG"
    set +e
    # Invoked through `bash` because a source mount can reject chmod; the
    # committed-mode assertions above are what hold the operator-facing entry
    # points executable.
    FAKE_LOG="$CASE_LOG" \
    FAKE_PSQL_LOG="$CASE_PSQL_LOG" \
    FAKE_RESPONSES="$RESPONSES" \
    FAKE_SCENARIO="$scenario" \
    KUBECTL="$WORK/kubectl" \
    PSQL="$WORK/psql" \
    DJINN_PREFLIGHT_DATABASE_URL='postgres://fake-preflight/djinn' \
    bash "$SCRIPT_UNDER_TEST" --context fake-context --mode "$mode" >"$CASE_OUTPUT" 2>&1
    local status=$?
    set -e
    if [ "$status" -ne "$expected_exit" ]; then
        cat "$CASE_OUTPUT" >&2
        fail "$tag exited $status, expected $expected_exit"
    fi
    local text
    for text in "$@"; do
        grep -Fq -- "$text" "$CASE_OUTPUT" || { cat "$CASE_OUTPUT" >&2; fail "$tag lacked diagnostic: $text"; }
    done
    if [ "$expected_exit" -eq 0 ]; then
        printf 'ACCEPTED (expected): %-34s mode=%-8s exit=0\n' "$scenario" "$mode"
    else
        printf 'REJECTED (expected): %-34s mode=%-8s exit=%s  %s\n' \
            "$scenario" "$mode" "$status" "$(grep -m1 '^FAIL: ' "$CASE_OUTPUT" | cut -c1-120)"
    fi
}

# --- The fence holds: the preflight is not simply always red. ---------------
run_case cutover-clear cutover 0 \
    'PASS: RuntimeClass djinn-cgroup-writable exists' \
    'PASS: zero live build-capable task-run Jobs in namespace djinn' \
    'PASS: zero live graph-warm Jobs in namespace djinn' \
    'PASS: zero Kueue Workloads in namespace djinn' \
    'PASS: zero nonterminal build_leases rows' \
    'PASS: Kueue cutover preflight fence holds for context fake-context namespace djinn'
CLEAR_LOG="$CASE_LOG"
CLEAR_PSQL_LOG="$CASE_PSQL_LOG"

# --- What the preflight actually ASKED. A gate that queries nothing passes
# every fixture; these assertions are what make the PASS lines above mean
# something. ------------------------------------------------------------------
grep -Fq -- 'get runtimeclasses.node.k8s.io' "$CLEAR_LOG" || fail 'preflight never listed the cluster RuntimeClasses'
grep -Fq -- 'get namespace djinn' "$CLEAR_LOG" || fail 'preflight never read the namespace labels'
grep -Fq -- 'djinn.app/component=task-run-worker' "$CLEAR_LOG" || fail 'preflight never queried task-run Jobs by the renderer label'
grep -Fq -- 'djinn.app/component=graph-warm' "$CLEAR_LOG" || fail 'preflight never queried graph-warm Jobs by the renderer label'
grep -Fq -- 'get workloads.kueue.x-k8s.io -n djinn' "$CLEAR_LOG" || fail 'preflight never queried Kueue Workloads'
grep -Fq -- 'FROM build_leases' "$CLEAR_PSQL_LOG" || fail 'preflight never queried the build_leases ledger'
grep -Fq -- "state <> '"'terminal'"'" "$CLEAR_PSQL_LOG" || fail 'the ledger query does not fence on nonterminal state'

# --- Read-only. The task adds GATES only: no namespace is labelled, no
# rendering changes, no authority moves. Asserted on the invocation log rather
# than on the source text, so it holds for whatever the script actually runs.
assert_read_only() {
    local log=$1
    local offending
    offending=$(grep '^kubectl ' "$log" | grep -v -- ' get ' || true)
    if [ -n "$offending" ]; then
        printf '%s\n' "$offending" >&2
        return 1
    fi
    offending=$(grep -E '^kubectl .* (apply|create|patch|label|annotate|delete|replace|edit|scale|drain|cordon|uncordon)( |$)' "$log" || true)
    if [ -n "$offending" ]; then
        printf '%s\n' "$offending" >&2
        return 1
    fi
    return 0
}
assert_read_only "$CLEAR_LOG" || fail 'preflight issued a non-get kubectl verb; it must never mutate the cluster'
printf 'PASS: every kubectl invocation on the clear path is a read-only get\n'

# The read-only assertion, proven non-vacuous. A mutant that labels the
# namespace must be caught; if this ever stops failing, the check above is
# decoration.
MUTANT="$WORK/preflight-mutant.sh"
awk '/^printf .INFO: namespace %s %s label/ {
        print "\"$KUBECTL\" --context \"$CONTEXT\" label namespace \"$NAMESPACE\" djinn.io/kueue-managed=true"
     }
     { print }' "$PREFLIGHT" >"$MUTANT"
grep -Fq -- 'label namespace' "$MUTANT" || fail 'read-only mutant was not constructed; the injection point moved'
SCRIPT_UNDER_TEST="$MUTANT"
# The injected call is a bare command, so the fake kubectl's exit 99 for an
# unmodelled invocation propagates under `set -e`. The exit code is incidental
# here — the invocation log is the subject.
run_case cutover-clear cutover 99
SCRIPT_UNDER_TEST="$PREFLIGHT"
if assert_read_only "$CASE_LOG" >/dev/null 2>&1; then
    fail 'the read-only assertion accepted a mutant that labels the namespace'
fi
printf 'REJECTED (expected): %-34s read-only assertion caught an injected `kubectl label namespace`\n' 'mutant/label-namespace'

# --- Gate 1: RuntimeClass ordering, both directions. ------------------------
run_case missing-runtimeclass cutover 10 \
    'FAIL: refusing to start the build-admission authority flip: RuntimeClass djinn-cgroup-writable is absent' \
    'server/crates/djinn-k8s/src/job.rs asserts' \
    'wedging all dispatch'
run_case missing-runtimeclass-labelled cutover 10 \
    'ALREADY carries djinn.io/kueue-managed=true while djinn-cgroup-writable is absent' \
    'dispatch is wedged NOW'
# Symmetric: the renderer assertion does not care which way authority is
# moving, so removing the RuntimeClass before the revert wedges dispatch just
# as surely.
run_case rollback-missing-runtimeclass rollback 10 \
    'FAIL: refusing to revert build-admission authority: RuntimeClass djinn-cgroup-writable is absent'

# --- Gate 2: the drain fence, one population at a time. ---------------------
run_case live-taskrun-job cutover 20 \
    'build-capable task-run Jobs are live in namespace djinn' \
    'djinn-taskrun-44444444-4444-4444-4444-444444444444 (active=1)'
# ... and the terminal Jobs in the same fixture must NOT be what tripped it.
if grep -Fq -- 'djinn-taskrun-11111111-1111-1111-1111-111111111111' "$WORK/live-taskrun-job-cutover.out"; then
    fail 'a Complete Job was reported as live'
fi
run_case live-warm-job cutover 21 \
    'graph-warm Jobs are live in namespace djinn' \
    'djinn-warm-djinn-55555555 (active=1)' \
    'build_leases.consumer_kind = graph_warm'
run_case workload-present cutover 22 \
    'Kueue already holds Workloads in namespace djinn' \
    'djinn-taskrun-44444444-4444-4444-4444-444444444444-abcde'

# Terminal Jobs are not live. Without this the drain fence could be satisfied
# by "no Jobs exist at all", which is a different and much rarer cluster.
run_case cutover-terminal-jobs-only cutover 0 \
    'PASS: zero live build-capable task-run Jobs in namespace djinn' \
    'PASS: zero live graph-warm Jobs in namespace djinn'

# --- Gate 3: the ledger fence. ----------------------------------------------
run_case occupying-lease cutover 30 \
    'build_leases carries nonterminal rows' \
    'task_invocation|active|1' \
    'OCCUPYING rows'
# A queued row is nonterminal too: it is an admission decision the flip would
# hand to a controller that never saw it enqueued.
run_case queued-lease-only cutover 30 \
    'build_leases carries nonterminal rows' \
    'graph_warm|queued|2'
run_case terminal-leases-only cutover 0 'PASS: zero nonterminal build_leases rows'

# --- Fail closed: an unreadable population is never an empty one. -----------
run_case ledger-unreadable cutover 40 \
    'the build-lease ledger could not be read' \
    'This is a failure, not a skip'
run_case api-error cutover 41 \
    'could not read Kueue Workloads in namespace djinn'

# --- Rollback symmetry. The same fence, refusing in the other direction. ----
run_case rollback-clear rollback 0 \
    'PASS: Kueue rollback preflight fence holds for context fake-context namespace djinn'
run_case rollback-live-taskrun-job rollback 20 \
    'FAIL: refusing to revert build-admission authority: the drain fence does not hold' \
    'djinn-taskrun-44444444-4444-4444-4444-444444444444 (active=1)'
run_case rollback-occupying-lease rollback 30 \
    'FAIL: refusing to revert build-admission authority: the drain fence does not hold' \
    'task_invocation|bound|1' \
    'stale occupying rows must be explicitly cleared before dispatch resumes'

# --- Caller errors. A preflight that guesses a direction, or reports a clear
# ledger it could not reach, is the failure mode this whole file exists for. --
expect_usage_rejection() {
    local name=$1
    shift
    local output="$WORK/usage-$name.out"
    set +e
    env "$@" bash "$PREFLIGHT" >"$output" 2>&1
    local status=$?
    set -e
    [ "$status" -eq 2 ] || { cat "$output" >&2; fail "usage case $name exited $status, expected 2"; }
    printf 'REJECTED (expected): %-34s exit=2  %s\n' "$name" "$(grep -m1 '^FAIL: ' "$output" | cut -c1-100)"
}

BASE_ENV=(KUBECTL="$WORK/kubectl" PSQL="$WORK/psql" FAKE_LOG=/dev/null FAKE_PSQL_LOG=/dev/null
          FAKE_RESPONSES="$RESPONSES" FAKE_SCENARIO=cutover-clear
          DJINN_PREFLIGHT_DATABASE_URL='postgres://fake-preflight/djinn')
expect_usage_rejection missing-context "${BASE_ENV[@]}" KUEUE_PREFLIGHT_MODE=cutover
expect_usage_rejection missing-mode "${BASE_ENV[@]}" KUEUE_PREFLIGHT_CONTEXT=fake-context
expect_usage_rejection unknown-mode "${BASE_ENV[@]}" KUEUE_PREFLIGHT_CONTEXT=fake-context KUEUE_PREFLIGHT_MODE=maybe
expect_usage_rejection missing-database-url -u DJINN_PREFLIGHT_DATABASE_URL \
    KUBECTL="$WORK/kubectl" PSQL="$WORK/psql" FAKE_LOG=/dev/null FAKE_PSQL_LOG=/dev/null \
    FAKE_RESPONSES="$RESPONSES" FAKE_SCENARIO=cutover-clear \
    KUEUE_PREFLIGHT_CONTEXT=fake-context KUEUE_PREFLIGHT_MODE=cutover
grep -Fq -- 'the build-lease fence cannot be attested without it' "$WORK/usage-missing-database-url.out" \
    || fail 'missing ledger connection was rejected for the wrong reason'

printf 'PASS: kueue preflight fake-kubectl contract completed\n'
