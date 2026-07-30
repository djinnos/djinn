#!/usr/bin/env bash
# Hermetic contract for zero-capture-gate.sh. No cluster credentials required.
# Usage: deploy/kueue/tests/zero-capture-gate.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KUEUE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$KUEUE_DIR/../.." && pwd)"
GATE="$KUEUE_DIR/zero-capture-gate.sh"
RESPONSES="$SCRIPT_DIR/fixtures/zero-capture"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

git -C "$REPO_ROOT" ls-files -s deploy/kueue/zero-capture-gate.sh | grep -Eq '^100755 ' || fail 'gate is not committed executable'
git -C "$REPO_ROOT" ls-files -s deploy/kueue/tests/zero-capture-gate.sh | grep -Eq '^100755 ' || fail 'contract script is not committed executable'
FIXTURE="$SCRIPT_DIR/fixtures/precutover-task-run.yaml"
[ -f "$FIXTURE" ] || fail "pre-cutover fixture is missing: $FIXTURE"
if grep -Eq '^[[:space:]]+djinn\.io/kueue-' "$FIXTURE"; then
    fail 'pre-cutover fixture must not carry a Kueue namespace or build-object label'
fi
cat >"$WORK/kubectl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'kubectl' >>"$FAKE_LOG"
printf ' %q' "$@" >>"$FAKE_LOG"
printf '\n' >>"$FAKE_LOG"
args=" $* "
if [[ "$args" == *' get workloads '* ]] && [ "$FAKE_SCENARIO" = api-error ]; then
    printf 'simulated Kubernetes API error\n' >&2
    exit 47
fi
if [[ "$args" == *' get namespace djinn '* ]]; then
    cat "$FAKE_RESPONSES/$FAKE_SCENARIO/namespace-label.txt"
elif [[ "$args" == *' get pods '* ]]; then
    cat "$FAKE_RESPONSES/$FAKE_SCENARIO/pod-phase.txt"
elif [[ "$args" == *' get workloads '* ]]; then
    cat "$FAKE_RESPONSES/$FAKE_SCENARIO/workloads.txt"
fi
EOF
cat >"$WORK/helm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'helm' >>"$FAKE_LOG"
printf ' %q' "$@" >>"$FAKE_LOG"
printf '\n' >>"$FAKE_LOG"
EOF
chmod +x "$WORK/kubectl" "$WORK/helm"

run_case() {
    local scenario=$1 expected_status=$2 expected_text=$3
    local output="$WORK/$scenario.out"
    : >"$WORK/$scenario.log"
    set +e
    # Run through bash because source mounts can reject chmod; the checks above
    # still require the committed operator-facing entry points to be executable.
    FAKE_LOG="$WORK/$scenario.log" \
    FAKE_RESPONSES="$RESPONSES" \
    FAKE_SCENARIO="$scenario" \
    KUBECTL="$WORK/kubectl" \
    HELM="$WORK/helm" \
    KUEUE_GATE_POLL_SECONDS=0 \
    bash "$GATE" --context fake-disposable --designated-operator-secret fake-designated-operator --timeout-seconds 1 >"$output" 2>&1
    local status=$?
    set -e
    if [ "$expected_status" = pass ]; then
        [ "$status" -eq 0 ] || { cat "$output" >&2; fail "$scenario unexpectedly failed"; }
    else
        [ "$status" -ne 0 ] || { cat "$output" >&2; fail "$scenario unexpectedly passed"; }
    fi
    grep -Fq -- "$expected_text" "$output" || { cat "$output" >&2; fail "$scenario lacked diagnostic: $expected_text"; }
}

run_case success pass 'PASS: zero-capture prerequisite gate completed'
SUCCESS_LOG="$WORK/success.log"
CHART_DIR="$(cd "$KUEUE_DIR/../helm/djinn" && pwd)"
grep -Fq -- "apply -f $KUEUE_DIR/vendor/kueue-v0.10.0.yaml" "$SUCCESS_LOG" || fail 'success did not apply pinned Kueue asset'
grep -Fq -- "upgrade --install djinn-inert-kueue-gate $CHART_DIR" "$SUCCESS_LOG" || fail 'success did not install inert Djinn chart'
grep -Fq -- 'migration.designatedOperatorSecret=fake-designated-operator' "$SUCCESS_LOG" || fail 'success did not provide the fresh-install designated operator Secret'
grep -Fq -- "apply -n djinn -f $SCRIPT_DIR/fixtures/precutover-task-run.yaml" "$SUCCESS_LOG" || fail 'success did not apply pre-cutover fixture'
grep -Fq -- 'get workloads -n djinn' "$SUCCESS_LOG" || fail 'success did not query namespace Workloads'

run_case managed-namespace fail 'namespace djinn is labelled djinn.io/kueue-managed=true'
run_case pod-pending fail 'fixture Pod did not reach Running within 1s (last phase: Pending)'
run_case workload-captured fail 'Kueue captured a Workload in namespace djinn'
run_case api-error fail 'kubectl API error while running: kubectl --context fake-disposable get workloads -n djinn'

MISSING_SECRET_OUTPUT="$WORK/missing-secret.out"
set +e
KUBECTL="$WORK/kubectl" HELM="$WORK/helm" bash "$GATE" --context fake-disposable >"$MISSING_SECRET_OUTPUT" 2>&1
MISSING_SECRET_STATUS=$?
set -e
[ "$MISSING_SECRET_STATUS" -ne 0 ] || fail 'gate unexpectedly accepted a fresh install without a designated operator Secret'
grep -Fq -- 'a caller-provided --designated-operator-secret is required for a fresh chart install' "$MISSING_SECRET_OUTPUT" || { cat "$MISSING_SECRET_OUTPUT" >&2; fail 'missing Secret rejection lacked diagnostic'; }
printf 'PASS: zero-capture gate fake-kubectl contract completed\n'
