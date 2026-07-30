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
POLL_SECONDS="${KUEUE_GATE_POLL_SECONDS:-2}"
RELEASE="${KUEUE_GATE_RELEASE:-djinn-inert-kueue-gate}"
DESIGNATED_OPERATOR_SECRET="${KUEUE_GATE_DESIGNATED_OPERATOR_SECRET:-}"
NAMESPACE="djinn"
JOB_NAME="zero-capture-precutover-task-run"
VENDORED_MANIFEST="$SCRIPT_DIR/vendor/kueue-v0.10.0.yaml"
CHART="$REPO_ROOT/deploy/helm/djinn"
FIXTURE="$SCRIPT_DIR/tests/fixtures/precutover-task-run.yaml"

usage() {
    cat >&2 <<'EOF'
Usage: deploy/kueue/zero-capture-gate.sh --context <disposable-context> --designated-operator-secret <secret-name> [options]

Installs the pinned Kueue manifest and inert Djinn chart in the supplied
context. It never labels the djinn namespace. This is a prerequisite-release
gate, not a cutover command.

Options:
  --context CONTEXT       Required disposable Kubernetes context.
  --designated-operator-secret NAME
                        Required caller-owned Secret name for the chart's
                        fresh-install migration bootstrap. Its contents are
                        never passed on the command line or read by this gate.
  --timeout-seconds N     Bounded Pod-running wait (default: 120).
  --release NAME          Helm release name (default: djinn-inert-kueue-gate).
  --help                  Show this message.

Environment overrides for automation: KUBECTL, HELM, KUEUE_GATE_CONTEXT,
KUEUE_GATE_TIMEOUT_SECONDS, KUEUE_GATE_POLL_SECONDS, KUEUE_GATE_RELEASE,
KUEUE_GATE_DESIGNATED_OPERATOR_SECRET.
EOF
}

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
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
[[ "$POLL_SECONDS" =~ ^[0-9]+$ ]] || fail 'KUEUE_GATE_POLL_SECONDS must be a non-negative integer'
[ -f "$VENDORED_MANIFEST" ] || fail "pinned Kueue manifest is missing: $VENDORED_MANIFEST"
[ -d "$CHART" ] || fail "Djinn chart is missing: $CHART"
[ -f "$FIXTURE" ] || fail "pre-cutover fixture is missing: $FIXTURE"

run_kubectl() {
    if ! "$KUBECTL" --context "$CONTEXT" "$@"; then
        fail "kubectl API error while running: kubectl --context $CONTEXT $*"
    fi
}

run_helm() {
    if ! "$HELM" --kube-context "$CONTEXT" "$@"; then
        fail "helm error while installing inert chart into context $CONTEXT"
    fi
}

diagnostics() {
    printf '%s\n' 'DIAGNOSTICS: namespace, fixture Job/Pods, and Workloads follow:' >&2
    "$KUBECTL" --context "$CONTEXT" get namespace "$NAMESPACE" -o yaml >&2 || true
    "$KUBECTL" --context "$CONTEXT" get job "$JOB_NAME" -n "$NAMESPACE" -o yaml >&2 || true
    "$KUBECTL" --context "$CONTEXT" get pods -n "$NAMESPACE" -l "job-name=$JOB_NAME" -o wide >&2 || true
    "$KUBECTL" --context "$CONTEXT" get workloads -n "$NAMESPACE" -o wide >&2 || true
}

printf 'INFO: installing pinned Kueue asset into context %s\n' "$CONTEXT"
run_kubectl apply -f "$VENDORED_MANIFEST"
printf 'INFO: installing inert Djinn chart release %s\n' "$RELEASE"
run_helm upgrade --install "$RELEASE" "$CHART" --namespace "$NAMESPACE" --create-namespace --wait --timeout "${TIMEOUT_SECONDS}s" --set-string "migration.designatedOperatorSecret=$DESIGNATED_OPERATOR_SECRET"

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
