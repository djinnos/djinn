#!/usr/bin/env bash
# Verify that the inert Kueue prerequisite release captures no existing task-run.
# Usage: deploy/kueue/zero-capture-gate.sh --context <disposable-context> --designated-operator-secret <secret-name>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
KUBECTL="${KUBECTL:-kubectl}"
HELM="${HELM:-helm}"
CONTEXT="${KUEUE_GATE_CONTEXT:-}"
TIMEOUT_SECONDS="${KUEUE_GATE_TIMEOUT_SECONDS:-120}"
# The chart install and the fixture Pod wait are different problems with
# different orders of magnitude, so they get separate budgets. One shared
# 120s value fails a fresh Djinn install on every cold cluster: the install
# has to pull djinn-server, Postgres and qdrant, bind three PVCs, run the
# designated-operator bootstrap and the schema migration, and only then can
# the server Deployment go Available. The fixture Pod, by contrast, is a
# `pause` image that must appear in seconds — a long budget there would just
# delay a genuine capture failure.
INSTALL_TIMEOUT_SECONDS="${KUEUE_GATE_INSTALL_TIMEOUT_SECONDS:-900}"
POLL_SECONDS="${KUEUE_GATE_POLL_SECONDS:-2}"
RELEASE="${KUEUE_GATE_RELEASE:-djinn-inert-kueue-gate}"
PREREQS_RELEASE="${KUEUE_GATE_PREREQS_RELEASE:-djinn-prereqs}"
PREREQS_NAMESPACE="${KUEUE_GATE_PREREQS_NAMESPACE:-kueue-system}"
DESIGNATED_OPERATOR_SECRET="${KUEUE_GATE_DESIGNATED_OPERATOR_SECRET:-}"
NAMESPACE="djinn"
JOB_NAME="zero-capture-precutover-task-run"
# The prerequisite is a PINNED UPSTREAM CHART, not a byte-vendored manifest.
# Applying a static YAML here would prove the gate against a file no consumer
# installs; deploy/kueue/vendor/ was retired for exactly that reason.
PREREQS_CHART="$REPO_ROOT/deploy/helm/djinn-prereqs"
CHART="$REPO_ROOT/deploy/helm/djinn"
FIXTURE="$SCRIPT_DIR/tests/fixtures/precutover-task-run.yaml"
# Operator-supplied chart values, forwarded verbatim to the Djinn chart install.
CHART_VALUE_ARGS=()

usage() {
    cat >&2 <<'EOF'
Usage: deploy/kueue/zero-capture-gate.sh --context <disposable-context> --designated-operator-secret <secret-name> [options]

Installs the pinned Kueue prerequisite chart (deploy/helm/djinn-prereqs) and
the inert Djinn chart in the supplied context. It never labels the djinn
namespace. This is a prerequisite-release gate, not a cutover command.

The Djinn chart's committed defaults are a multi-node production shape
(ReadWriteMany volumes, an unqualified `djinn-server:latest` image). No
disposable cluster satisfies them and production does not run them either, so
the release values are the caller's to supply with --values/--set. Everything
except the two gate-owned keys below is forwarded verbatim.

Options:
  --context CONTEXT       Required disposable Kubernetes context.
  --designated-operator-secret NAME
                        Required caller-owned Secret name for the chart's
                        fresh-install migration bootstrap. Its contents are
                        never passed on the command line or read by this gate.
                        It must already exist in the djinn namespace; the gate
                        creates that namespace but never that Secret.
  --values FILE, -f FILE  Values file forwarded to the Djinn chart install.
                        Repeatable, merged by Helm in the order given.
  --set KEY=VALUE         Forwarded to the Djinn chart install. Repeatable.
  --set-string KEY=VALUE  Forwarded to the Djinn chart install. Repeatable.
  --install-timeout-seconds N
                        Budget for each `helm upgrade --install --wait`
                        (default: 900). Covers image pulls, PVC binding and
                        the schema migration on a cold cluster.
  --timeout-seconds N     Bounded fixture Pod-running wait (default: 120).
                        This does NOT govern the chart installs.
  --release NAME          Helm release name (default: djinn-inert-kueue-gate).
  --help                  Show this message.

`kueue.enabled` and `migration.designatedOperatorSecret` are owned by this gate
and are rejected as caller overrides: the first is what makes the run
meaningful and the second is already a first-class flag.

Environment overrides for automation: KUBECTL, HELM, KUEUE_GATE_CONTEXT,
KUEUE_GATE_TIMEOUT_SECONDS, KUEUE_GATE_INSTALL_TIMEOUT_SECONDS,
KUEUE_GATE_POLL_SECONDS, KUEUE_GATE_RELEASE, KUEUE_GATE_PREREQS_RELEASE,
KUEUE_GATE_PREREQS_NAMESPACE, KUEUE_GATE_DESIGNATED_OPERATOR_SECRET.
EOF
}

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

# Helm applies every --set/--set-string on top of every --values file, so a
# caller values file can never quietly turn the queue topology off. A caller
# --set can, and a gate that installed `kueue.enabled=false` would report
# "zero Workloads captured" for the trivial reason that no queue exists —
# the exact shape of green-against-nothing this gate is supposed to refute.
reject_gate_owned_key() {
    local flag=$1 assignment=$2 key=${2%%=*}
    case "$key" in
        kueue|kueue.*|migration.designatedOperatorSecret)
            fail "$flag $assignment overrides a gate-owned key ($key); kueue.* is what makes this gate meaningful and migration.designatedOperatorSecret has its own --designated-operator-secret flag"
            ;;
    esac
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --context)
            [ "$#" -ge 2 ] || fail '--context requires a value'
            CONTEXT=$2
            shift 2
            ;;
        --designated-operator-secret)
            [ "$#" -ge 2 ] || fail '--designated-operator-secret requires a value'
            DESIGNATED_OPERATOR_SECRET=$2
            shift 2
            ;;
        --values|-f)
            [ "$#" -ge 2 ] || fail '--values requires a value'
            [ -f "$2" ] || fail "--values file does not exist: $2"
            CHART_VALUE_ARGS+=(--values "$2")
            shift 2
            ;;
        --set)
            [ "$#" -ge 2 ] || fail '--set requires a value'
            reject_gate_owned_key --set "$2"
            CHART_VALUE_ARGS+=(--set "$2")
            shift 2
            ;;
        --set-string)
            [ "$#" -ge 2 ] || fail '--set-string requires a value'
            reject_gate_owned_key --set-string "$2"
            CHART_VALUE_ARGS+=(--set-string "$2")
            shift 2
            ;;
        --install-timeout-seconds)
            [ "$#" -ge 2 ] || fail '--install-timeout-seconds requires a value'
            INSTALL_TIMEOUT_SECONDS=$2
            shift 2
            ;;
        --timeout-seconds)
            [ "$#" -ge 2 ] || fail '--timeout-seconds requires a value'
            TIMEOUT_SECONDS=$2
            shift 2
            ;;
        --release)
            [ "$#" -ge 2 ] || fail '--release requires a value'
            RELEASE=$2
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *) fail "unknown option: $1" ;;
    esac
done

[ -n "$CONTEXT" ] || { usage; fail 'a caller-provided --context is required'; }
[ -n "$DESIGNATED_OPERATOR_SECRET" ] || { usage; fail 'a caller-provided --designated-operator-secret is required for a fresh chart install'; }
[[ "$DESIGNATED_OPERATOR_SECRET" =~ ^[a-z0-9]([-.a-z0-9]*[a-z0-9])?$ ]] || fail '--designated-operator-secret must be a valid Kubernetes Secret name'
[[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail '--timeout-seconds must be a positive integer'
[[ "$INSTALL_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail '--install-timeout-seconds must be a positive integer'
[[ "$POLL_SECONDS" =~ ^[0-9]+$ ]] || fail 'KUEUE_GATE_POLL_SECONDS must be a non-negative integer'
[ -d "$PREREQS_CHART" ] || fail "Kueue prerequisite chart is missing: $PREREQS_CHART"
[ -f "$PREREQS_CHART/Chart.lock" ] || fail "prerequisite chart is unpinned: $PREREQS_CHART/Chart.lock is missing"
# Vendored so the gate installs the exact reviewed bytes with no registry
# round-trip: a resolve-at-install-time dependency could hand the target cluster
# a different chart than the contracts validated.
ls "$PREREQS_CHART"/charts/kueue-*.tgz >/dev/null 2>&1 || fail "prerequisite chart has no vendored kueue dependency; run: helm dependency update $PREREQS_CHART"
[ -d "$CHART" ] || fail "Djinn chart is missing: $CHART"
[ -f "$FIXTURE" ] || fail "pre-cutover fixture is missing: $FIXTURE"

run_kubectl() {
    if ! "$KUBECTL" --context "$CONTEXT" "$@"; then
        fail "kubectl API error while running: kubectl --context $CONTEXT $*"
    fi
}

run_helm() {
    if ! "$HELM" --kube-context "$CONTEXT" "$@"; then
        fail "helm error while installing into context $CONTEXT: helm --kube-context $CONTEXT $*"
    fi
}

diagnostics() {
    printf '%s\n' 'DIAGNOSTICS: namespace, fixture Job/Pods, and Workloads follow:' >&2
    "$KUBECTL" --context "$CONTEXT" get namespace "$NAMESPACE" -o yaml >&2 || true
    "$KUBECTL" --context "$CONTEXT" get job "$JOB_NAME" -n "$NAMESPACE" -o yaml >&2 || true
    "$KUBECTL" --context "$CONTEXT" get pods -n "$NAMESPACE" -l "job-name=$JOB_NAME" -o wide >&2 || true
    "$KUBECTL" --context "$CONTEXT" get workloads -n "$NAMESPACE" -o wide >&2 || true
}

# The chart renders the Namespace itself (namespace.create defaults true), and
# that rendered object is exactly what assertion 1 below inspects — so this gate
# must NOT install with namespace.create=false. But the caller-owned
# designated-operator Secret has to exist inside that namespace before the
# install, because the server Pod's bootstrap initContainer resolves it through
# secretKeyRef and never starts otherwise. Those two facts deadlock on a fresh
# cluster: you cannot create the Secret without the namespace, and Helm refuses
# to adopt a namespace it did not create ("exists and cannot be imported into
# the current release: invalid ownership metadata").
#
# Stamping Helm's ownership metadata onto the namespace is the documented way
# out of that adoption refusal, and it is why --context must be disposable: the
# stamp reassigns the namespace to this release. The chart still owns and
# re-renders the Namespace object, so the inertness assertion still reads a
# chart-produced object rather than a hand-made one.
prepare_namespace_for_adoption() {
    local create_output
    if create_output=$("$KUBECTL" --context "$CONTEXT" create namespace "$NAMESPACE" 2>&1); then
        printf 'INFO: created namespace %s so the caller-owned designated-operator Secret can precede the install\n' "$NAMESPACE"
    elif printf '%s' "$create_output" | grep -q 'already exists'; then
        printf 'INFO: namespace %s already exists; adopting it into release %s\n' "$NAMESPACE" "$RELEASE"
    else
        printf '%s\n' "$create_output" >&2
        fail "could not create namespace $NAMESPACE in context $CONTEXT"
    fi
    run_kubectl label namespace "$NAMESPACE" app.kubernetes.io/managed-by=Helm --overwrite
    run_kubectl annotate namespace "$NAMESPACE" \
        "meta.helm.sh/release-name=$RELEASE" \
        "meta.helm.sh/release-namespace=$NAMESPACE" --overwrite
}

printf 'INFO: installing pinned Kueue prerequisite release %s into context %s\n' "$PREREQS_RELEASE" "$CONTEXT"
run_helm upgrade --install "$PREREQS_RELEASE" "$PREREQS_CHART" --namespace "$PREREQS_NAMESPACE" --create-namespace --wait --timeout "${INSTALL_TIMEOUT_SECONDS}s"

prepare_namespace_for_adoption

# Fail here, with the recipe, rather than 900s later with a Pod stuck in
# CreateContainerConfigError that names no cause.
if ! "$KUBECTL" --context "$CONTEXT" get secret "$DESIGNATED_OPERATOR_SECRET" -n "$NAMESPACE" >/dev/null 2>&1; then
    fail "designated-operator Secret $DESIGNATED_OPERATOR_SECRET is absent from namespace $NAMESPACE; the chart's bootstrap initContainer resolves it via secretKeyRef and the install would hang. Create it first (the gate never reads, creates or prints its contents):
  kubectl --context $CONTEXT -n $NAMESPACE create secret generic $DESIGNATED_OPERATOR_SECRET \\
    --from-literal=user_id=<uuid> --from-literal=github_id=<numeric-github-id> --from-literal=github_login=<login>"
fi

printf 'INFO: installing inert Djinn chart release %s\n' "$RELEASE"
# kueue.enabled=true is what makes this gate meaningful: the Djinn queue
# topology only renders on request, and the gate exists to prove that rendering
# it alongside the prerequisite still captures nothing. It is set LAST so that
# it also wins against any caller --set that slipped past reject_gate_owned_key.
run_helm upgrade --install "$RELEASE" "$CHART" --namespace "$NAMESPACE" --create-namespace --wait --timeout "${INSTALL_TIMEOUT_SECONDS}s" \
    ${CHART_VALUE_ARGS[@]+"${CHART_VALUE_ARGS[@]}"} \
    --set kueue.enabled=true --set-string "migration.designatedOperatorSecret=$DESIGNATED_OPERATOR_SECRET"

namespace_label=$(run_kubectl get namespace "$NAMESPACE" -o 'jsonpath={.metadata.labels.djinn\.io/kueue-managed}')
if [ "$namespace_label" = 'true' ]; then
    diagnostics
    fail "namespace $NAMESPACE is labelled djinn.io/kueue-managed=true; the prerequisite release is not inert"
fi
printf 'PASS: namespace %s is not Kueue-managed\n' "$NAMESPACE"

printf 'INFO: submitting unchanged pre-cutover task-run fixture %s\n' "$JOB_NAME"
run_kubectl apply -n "$NAMESPACE" -f "$FIXTURE"
started_at=$SECONDS
last_phase='<no Pod observed>'
while :; do
    phase=$(run_kubectl get pods -n "$NAMESPACE" -l "job-name=$JOB_NAME" -o 'jsonpath={.items[0].status.phase}')
    last_phase=${phase:-'<no Pod observed>'}
    if [ "$phase" = 'Running' ]; then
        printf 'PASS: fixture Pod reached Running\n'
        break
    fi
    if [ $((SECONDS - started_at)) -ge "$TIMEOUT_SECONDS" ]; then
        diagnostics
        fail "fixture Pod did not reach Running within ${TIMEOUT_SECONDS}s (last phase: $last_phase)"
    fi
    sleep "$POLL_SECONDS"
done

workloads=$(run_kubectl get workloads -n "$NAMESPACE" -o 'jsonpath={range .items[*]}{.metadata.name}{"\n"}{end}')
if [ -n "$workloads" ]; then
    diagnostics
    fail "Kueue captured a Workload in namespace $NAMESPACE while testing fixture $JOB_NAME: $workloads"
fi
printf 'PASS: zero Workloads captured in namespace %s\n' "$NAMESPACE"
printf 'PASS: zero-capture prerequisite gate completed for context %s\n' "$CONTEXT"
