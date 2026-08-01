#!/usr/bin/env bash
# Kueue cutover quiesce/rollback preflight.
#
# WHAT THIS IS
# ------------
# The executable half of deploy/runbooks/kueue-cutover-quiesce-rollback.md. It
# answers one question against a caller-provided context, in one of two
# directions:
#
#   --mode cutover   may the build-admission authority flip to Kueue now?
#   --mode rollback  may that flip be reverted now?
#
# It is READ-ONLY. It never labels a namespace, never installs or upgrades a
# release, never touches Job rendering or admission authority. Every kubectl
# invocation it makes is a `get`, and its own contract
# (deploy/kueue/tests/preflight.sh) fails if a mutating verb ever appears in
# the command log.
#
# WHY IT EXISTS
# -------------
# Two independent fail-closed gates stack on the cutover:
#
# 1. ORDERING. `server/crates/djinn-k8s/src/job.rs` asserts
#    "required cgroup launcher requires runtimeClassName: djinn-cgroup-writable"
#    — an `assert!`, i.e. a renderer PANIC, not a rejected Job. Labelling a
#    namespace `djinn.io/kueue-managed=true` on a cluster whose RuntimeClass
#    `djinn-cgroup-writable` is absent therefore wedges ALL dispatch: Kueue
#    gates the Job, and the renderer that would produce the Job panics first.
#    That exact class of failure took production down twice (v0.7.25, armed
#    launcher without RuntimeClass; and again on 2026-07-30). The RuntimeClass
#    must EXIST BEFORE the namespace may carry the label, and that ordering is
#    asserted here rather than remembered.
#
# 2. DRAIN. An authority-mode flip is only safe across a quiesced fleet. A
#    task-run Job that is live while authority moves is adjudicated by neither
#    ledger — the Postgres one it started under, nor the Kueue one it lands in.
#    The fence is zero live build-capable task-run Jobs, zero live graph-warm
#    Jobs, zero Kueue Workloads in the namespace, and zero nonterminal rows in
#    the `build_leases` ledger.
#
# The fence is SYMMETRIC. Reverting authority across a live fleet strands the
# same work in the mirror image, so --mode rollback enforces exactly the same
# populations and refuses to proceed when any of them is nonzero.
#
# WHY EVERY POPULATION IS CHECKED SEPARATELY
# ------------------------------------------
# Each has its own exit code and its own diagnostic. A single "not drained"
# verdict tells an operator to go look; a named population tells them what to
# wait for. The codes are also what makes the hermetic contract non-vacuous:
# a fixture proves a specific check fired, not merely that something failed.
#
#   0   the fence holds in the requested direction
#   2   usage / caller error
#   10  RuntimeClass djinn-cgroup-writable is absent (the ordering gate)
#   20  live build-capable task-run Jobs
#   21  live graph-warm Jobs
#   22  Kueue Workloads present in the namespace
#   30  nonterminal build_leases rows
#   40  the build-lease ledger could not be read (fail closed)
#   41  the Kubernetes API could not be read (fail closed)
#
# 40 and 41 are failures, never skips. A preflight that reports a clear fence
# because it could not see one of the populations is worse than no preflight:
# it manufactures the evidence the cutover is waiting for.
#
# USAGE
#   deploy/kueue/preflight.sh --context <context> --mode cutover|rollback
#                             [--namespace djinn]
#
# The build-lease ledger connection is read from DJINN_PREFLIGHT_DATABASE_URL,
# never from a command-line flag, so it does not land in this process's argv.
#
# Environment overrides for automation: KUBECTL, PSQL,
# KUEUE_PREFLIGHT_CONTEXT, KUEUE_PREFLIGHT_MODE, KUEUE_PREFLIGHT_NAMESPACE.
set -euo pipefail

KUBECTL="${KUBECTL:-kubectl}"
PSQL="${PSQL:-psql}"
CONTEXT="${KUEUE_PREFLIGHT_CONTEXT:-}"
MODE="${KUEUE_PREFLIGHT_MODE:-}"
# Which namespace would carry djinn.io/kueue-managed. Defaults to djinn; the
# cutover epic may instead choose a dedicated build-Job namespace (see the
# residual-risk section of deploy/kueue/README.md), and this preflight must be
# usable against whichever it picks without an edit.
NAMESPACE="${KUEUE_PREFLIGHT_NAMESPACE:-djinn}"

RUNTIME_CLASS='djinn-cgroup-writable'
MANAGED_LABEL='djinn.io/kueue-managed'
TASK_RUN_SELECTOR='djinn.app/component=task-run-worker'
WARM_SELECTOR='djinn.app/component=graph-warm'

EXIT_USAGE=2
EXIT_MISSING_RUNTIME_CLASS=10
EXIT_LIVE_TASK_RUN_JOBS=20
EXIT_LIVE_WARM_JOBS=21
EXIT_WORKLOADS_PRESENT=22
EXIT_NONTERMINAL_LEASES=30
EXIT_LEDGER_UNREADABLE=40
EXIT_API_UNREADABLE=41

usage() {
    cat >&2 <<'EOF'
Usage: deploy/kueue/preflight.sh --context <context> --mode cutover|rollback [options]

Read-only quiesce fence for the Kueue build-admission cutover. It asserts, in
the requested direction, that the RuntimeClass ordering holds and that nothing
build-capable is in flight. It changes nothing.

Options:
  --context CONTEXT     Required Kubernetes context to inspect.
  --mode MODE           Required: cutover (before the authority flip) or
                        rollback (before reverting it).
  --namespace NAME      Namespace that would carry djinn.io/kueue-managed
                        (default: djinn).
  --help                Show this message.

Required environment:
  DJINN_PREFLIGHT_DATABASE_URL  libpq connection string for the Djinn database,
                        used read-only to count nonterminal build_leases rows.
                        Read from the environment so it never appears in this
                        script's argv.

Environment overrides for automation: KUBECTL, PSQL, KUEUE_PREFLIGHT_CONTEXT,
KUEUE_PREFLIGHT_MODE, KUEUE_PREFLIGHT_NAMESPACE.
EOF
}

# Every failure exits with its own code so a caller (and the hermetic contract)
# can tell which gate fired without parsing prose.
fail() {
    local code=$1
    shift
    printf 'FAIL: %s\n' "$*" >&2
    exit "$code"
}

# The direction-appropriate refusal. The fence is the same in both modes; what
# differs is the sentence an operator reads at 3am.
refusal() {
    if [ "$MODE" = rollback ]; then
        printf 'refusing to revert build-admission authority'
    else
        printf 'refusing to start the build-admission authority flip'
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --context)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--context requires a value'
            CONTEXT=$2
            shift 2
            ;;
        --mode)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--mode requires a value'
            MODE=$2
            shift 2
            ;;
        --namespace)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--namespace requires a value'
            NAMESPACE=$2
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *) fail "$EXIT_USAGE" "unknown option: $1" ;;
    esac
done

[ -n "$CONTEXT" ] || { usage; fail "$EXIT_USAGE" 'a caller-provided --context is required'; }
case "$MODE" in
    cutover|rollback) ;;
    '') usage; fail "$EXIT_USAGE" 'a caller-provided --mode is required (cutover or rollback)' ;;
    *) usage; fail "$EXIT_USAGE" "--mode must be cutover or rollback, not: $MODE" ;;
esac
[[ "$NAMESPACE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || fail "$EXIT_USAGE" '--namespace must be a valid Kubernetes namespace name'
# A preflight that cannot read the ledger must say so before it reads anything
# else, rather than reporting three clear populations and then discovering it
# was never able to check the fourth.
[ -n "${DJINN_PREFLIGHT_DATABASE_URL:-}" ] || {
    usage
    fail "$EXIT_USAGE" 'DJINN_PREFLIGHT_DATABASE_URL is required; the build-lease fence cannot be attested without it'
}

# A read-only kubectl call. Any nonzero status is a fail-closed API error, not
# an empty population: `get workloads` on a cluster whose Kueue CRDs are
# missing exits nonzero, and reading that as "zero Workloads" would pass the
# fence for the exact cluster that cannot host the cutover.
STDERR_CAPTURE="$(mktemp)"
trap 'rm -f "$STDERR_CAPTURE"' EXIT

get_kubectl() {
    local description=$1
    shift
    local output
    # stderr is captured separately rather than folded into stdout: a kubectl
    # deprecation warning must not be parsed as a Job name.
    if ! output=$("$KUBECTL" --context "$CONTEXT" "$@" 2>"$STDERR_CAPTURE"); then
        cat "$STDERR_CAPTURE" >&2
        fail "$EXIT_API_UNREADABLE" "could not read $description: kubectl --context $CONTEXT $*"
    fi
    printf '%s' "$output"
}

# --- Gate 1: the RuntimeClass ordering law. ---------------------------------
#
# Listed rather than fetched by name on purpose: `kubectl get runtimeclass X`
# exits nonzero for a missing object, which is indistinguishable at the shell
# from an unreachable API server. Listing separates "the cluster answered, and
# the class is not there" (exit 10) from "the cluster did not answer" (exit 41).
runtime_classes=$(get_kubectl 'the cluster RuntimeClasses' get runtimeclasses.node.k8s.io \
    -o 'jsonpath={range .items[*]}{.metadata.name}{"\n"}{end}')
namespace_label=$(get_kubectl "the $NAMESPACE namespace labels" get namespace "$NAMESPACE" \
    -o 'jsonpath={.metadata.labels.djinn\.io/kueue-managed}')

if printf '%s\n' "$runtime_classes" | grep -Fxq -- "$RUNTIME_CLASS"; then
    printf 'PASS: RuntimeClass %s exists; the %s label may be applied to namespace %s\n' \
        "$RUNTIME_CLASS" "$MANAGED_LABEL" "$NAMESPACE"
else
    printf 'DIAGNOSTICS: RuntimeClasses present in context %s:\n' "$CONTEXT" >&2
    printf '%s\n' "${runtime_classes:-<none>}" >&2
    if [ "$namespace_label" = 'true' ]; then
        # Not a hypothetical any more: the label is already on and the
        # RuntimeClass is not. Dispatch is wedged right now.
        printf 'DIAGNOSTICS: namespace %s ALREADY carries %s=true while %s is absent — dispatch is wedged NOW\n' \
            "$NAMESPACE" "$MANAGED_LABEL" "$RUNTIME_CLASS" >&2
    fi
    fail "$EXIT_MISSING_RUNTIME_CLASS" \
        "$(refusal): RuntimeClass $RUNTIME_CLASS is absent from context $CONTEXT. Install it (deploy/helm/djinn, cgroupWritable.runtimeClass.enabled=true) BEFORE namespace $NAMESPACE carries $MANAGED_LABEL: server/crates/djinn-k8s/src/job.rs asserts 'required cgroup launcher requires runtimeClassName: $RUNTIME_CLASS', so the Job renderer PANICS while Kueue holds the Job, wedging all dispatch"
fi

printf 'INFO: namespace %s %s label: %s\n' "$NAMESPACE" "$MANAGED_LABEL" "${namespace_label:-<unset>}"

# --- Gate 2: the drain fence, one population at a time. ---------------------
#
# A Job is LIVE unless the API server says it reached a terminal condition.
# `.status.active` alone is not enough: a Job whose Pod has not been created
# yet reports no active count and is very much in flight.
live_jobs() {
    local description=$1 selector=$2
    local rows
    # "|" rather than a tab on purpose: tab is IFS whitespace, so bash collapses
    # consecutive tabs into one delimiter and an absent `.status.active` shifts
    # every later field left — which reads a Complete Job as live.
    rows=$(get_kubectl "$description" get jobs -n "$NAMESPACE" -l "$selector" \
        -o 'jsonpath={range .items[*]}{.metadata.name}{"|"}{.status.active}{"|"}{.status.conditions[?(@.type=="Complete")].status}{"|"}{.status.conditions[?(@.type=="Failed")].status}{"\n"}{end}')
    printf '%s\n' "$rows" | while IFS='|' read -r name active complete failed; do
        [ -n "$name" ] || continue
        if [ "$complete" = 'True' ] || [ "$failed" = 'True' ]; then
            continue
        fi
        printf '%s (active=%s)\n' "$name" "${active:-0}"
    done
}

# EVERY live task-run Job counts as build-capable. The Job renderer emits no
# role label (job.rs writes only djinn.app/task-run-id and
# djinn.app/component), so the cluster cannot be asked which runs will
# compile. `RoleResourceClass::for_role` already fails safe to build-capable
# for an unknown role; this fence makes the same choice for the same reason.
# Inventing a parallel selector here would fence a set no renderer produces.
task_run_jobs=$(live_jobs 'live task-run Jobs' "$TASK_RUN_SELECTOR")
if [ -n "$task_run_jobs" ]; then
    printf 'DIAGNOSTICS: live build-capable task-run Jobs in namespace %s:\n%s\n' "$NAMESPACE" "$task_run_jobs" >&2
    fail "$EXIT_LIVE_TASK_RUN_JOBS" \
        "$(refusal): the drain fence does not hold — build-capable task-run Jobs are live in namespace $NAMESPACE (selector $TASK_RUN_SELECTOR). Pause dispatch and wait for them to reach a terminal condition."
fi
printf 'PASS: zero live build-capable task-run Jobs in namespace %s\n' "$NAMESPACE"

warm_jobs=$(live_jobs 'live graph-warm Jobs' "$WARM_SELECTOR")
if [ -n "$warm_jobs" ]; then
    printf 'DIAGNOSTICS: live graph-warm Jobs in namespace %s:\n%s\n' "$NAMESPACE" "$warm_jobs" >&2
    fail "$EXIT_LIVE_WARM_JOBS" \
        "$(refusal): the drain fence does not hold — graph-warm Jobs are live in namespace $NAMESPACE (selector $WARM_SELECTOR). Warm Jobs take build leases too (build_leases.consumer_kind = graph_warm), so they are inside the fence, not beside it."
fi
printf 'PASS: zero live graph-warm Jobs in namespace %s\n' "$NAMESPACE"

workloads=$(get_kubectl "Kueue Workloads in namespace $NAMESPACE" get workloads.kueue.x-k8s.io -n "$NAMESPACE" \
    -o 'jsonpath={range .items[*]}{.metadata.name}{"\n"}{end}')
workloads=$(printf '%s' "$workloads" | sed '/^$/d')
if [ -n "$workloads" ]; then
    printf 'DIAGNOSTICS: Kueue Workloads in namespace %s:\n%s\n' "$NAMESPACE" "$workloads" >&2
    fail "$EXIT_WORKLOADS_PRESENT" \
        "$(refusal): Kueue already holds Workloads in namespace $NAMESPACE. Before the flip they must not exist at all (the release is inert — see deploy/kueue/zero-capture-gate.sh); before a revert they must be drained, or reverting strands them with no admitting controller."
fi
printf 'PASS: zero Kueue Workloads in namespace %s\n' "$NAMESPACE"

# --- Gate 3: the ledger fence. ----------------------------------------------
#
# The SQL is the fence. `state <> 'terminal'` covers queued rows as well as
# occupying ones: a queued row is an admission decision the flip would hand to
# a controller that never saw it enqueued.
lease_sql="SELECT consumer_kind, state, count(*) FROM build_leases WHERE state <> 'terminal' GROUP BY consumer_kind, state ORDER BY consumer_kind, state"
if ! lease_rows=$("$PSQL" --dbname "$DJINN_PREFLIGHT_DATABASE_URL" --no-align --tuples-only \
    --field-separator '|' --command "$lease_sql" 2>"$STDERR_CAPTURE"); then
    cat "$STDERR_CAPTURE" >&2
    fail "$EXIT_LEDGER_UNREADABLE" \
        "$(refusal): the build-lease ledger could not be read, so the fence cannot be attested. This is a failure, not a skip — an unreadable ledger is indistinguishable from a full one."
fi
lease_rows=$(printf '%s' "$lease_rows" | sed '/^$/d')
if [ -n "$lease_rows" ]; then
    printf 'DIAGNOSTICS: nonterminal build_leases rows (consumer_kind|state|count):\n%s\n' "$lease_rows" >&2
    occupying=$(printf '%s\n' "$lease_rows" | grep -E '\|(granted|launching|bound|active|suspect)\|' || true)
    if [ -n "$occupying" ]; then
        printf 'DIAGNOSTICS: OCCUPYING rows (they hold capacity, and a revert that resumes over them double-books it):\n%s\n' "$occupying" >&2
    fi
    fail "$EXIT_NONTERMINAL_LEASES" \
        "$(refusal): the drain fence does not hold — build_leases carries nonterminal rows. Every row must reach state 'terminal' first; on a revert, stale occupying rows must be explicitly cleared before dispatch resumes, or the restored ledger admits against capacity it believes is already taken.
REMEDY: a lease whose owning Job has reached a terminal condition, or whose Job is gone, is retired automatically by the server's reclamation sweep within one settle window (default 300s) — wait before intervening. If a row survives that, clear it explicitly:
  djinn-server build-lease list
  djinn-server build-lease clear --lease <consumer_kind>:<consumer_id>
Do NOT hand-write an UPDATE against build_leases: 'clear' takes the ledger's advisory lock and records terminal_reason='operator_cleared', so the intervention is auditable and cannot race a live grant."
fi
printf 'PASS: zero nonterminal build_leases rows\n'

printf 'PASS: Kueue %s preflight fence holds for context %s namespace %s\n' "$MODE" "$CONTEXT" "$NAMESPACE"
