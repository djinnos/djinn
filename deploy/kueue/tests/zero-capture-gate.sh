#!/usr/bin/env bash
# Hermetic contract for zero-capture-gate.sh. No cluster credentials required.
# Usage: deploy/kueue/tests/zero-capture-gate.sh
#
# SCOPE — read this before quoting a pass anywhere.
# ------------------------------------------------
# `helm` and `kubectl` are both stubs here. NOTHING IS INSTALLED and no cluster
# is contacted, so the install path — the only part of the gate that can fail
# environmentally — is NOT exercised. A pass proves argument construction,
# argument ordering and failure handling; it is not evidence that the gate can
# run anywhere. The helm stub used to be a two-line log-and-exit-0, which is how
# three independent install-path blockers (namespace adoption, missing values
# passthrough, an unpullable default image) all shipped behind a green
# contract. It is now driven to fail on demand so the gate's install-failure
# handling is actually covered, and this script prints an explicit UNVERIFIED
# declaration at the end so a pass cannot be read as "the gate works".
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
# The runbook's target-cluster procedure cites this file by path. It is the
# difference between a gate that installs a shape no cluster provisions and one
# an operator can actually run.
SINGLE_NODE_VALUES="$SCRIPT_DIR/fixtures/single-node-values.yaml"
[ -f "$SINGLE_NODE_VALUES" ] || fail "single-node values file the runbook cites is missing: $SINGLE_NODE_VALUES"
grep -Fq 'ReadWriteOnce' "$SINGLE_NODE_VALUES" || fail 'single-node values do not override the chart ReadWriteMany defaults, which no single-node default StorageClass can bind'
if grep -Eq '^[[:space:]]+(server|runtime):[[:space:]]*[^[:space:]#]' "$SINGLE_NODE_VALUES"; then
    fail 'single-node values pin an image tag; the release under test belongs on the command line and in the release record'
fi

cat >"$WORK/kubectl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'kubectl' >>"$FAKE_LOG"
printf ' %q' "$@" >>"$FAKE_LOG"
printf '\n' >>"$FAKE_LOG"
args=" $* "
if [[ "$args" == *' create namespace djinn '* ]] && [ -n "${FAKE_NAMESPACE_EXISTS:-}" ]; then
    printf 'Error from server (AlreadyExists): namespaces "djinn" already exists\n' >&2
    exit 1
fi
if [[ "$args" == *' get secret '* ]]; then
    if [ -n "${FAKE_SECRET_MISSING:-}" ]; then
        printf 'Error from server (NotFound): secrets "fake-designated-operator" not found\n' >&2
        exit 1
    fi
    exit 0
fi
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
# This stub INSTALLS NOTHING. It records argv so the contract can assert on the
# arguments the gate builds, and it exits nonzero when a scenario asks it to so
# that the gate's install-failure handling is covered rather than assumed. What
# it records are still only arguments: no chart is rendered, no StorageClass is
# consulted and no image is pulled.
cat >"$WORK/helm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'helm' >>"$FAKE_LOG"
printf ' %q' "$@" >>"$FAKE_LOG"
printf '\n' >>"$FAKE_LOG"
args=" $* "
if [ -n "${FAKE_HELM_FAIL_MATCH:-}" ] && [[ "$args" == *"$FAKE_HELM_FAIL_MATCH"* ]]; then
    printf 'simulated helm install failure\n' >&2
    exit 1
fi
exit 0
EOF
chmod +x "$WORK/kubectl" "$WORK/helm"

# Scenario knobs, read by the stubs. Set them around a call, never globally.
FAKE_SCENARIO=success
FAKE_NAMESPACE_EXISTS=
FAKE_SECRET_MISSING=
FAKE_HELM_FAIL_MATCH=

run_gate() {
    local output=$1 log=$2 status=0
    shift 2
    : >"$log"
    FAKE_LOG="$log" \
    FAKE_RESPONSES="$RESPONSES" \
    FAKE_SCENARIO="$FAKE_SCENARIO" \
    FAKE_NAMESPACE_EXISTS="$FAKE_NAMESPACE_EXISTS" \
    FAKE_SECRET_MISSING="$FAKE_SECRET_MISSING" \
    FAKE_HELM_FAIL_MATCH="$FAKE_HELM_FAIL_MATCH" \
    KUBECTL="$WORK/kubectl" \
    HELM="$WORK/helm" \
    KUEUE_GATE_POLL_SECONDS=0 \
    bash "$GATE" "$@" >"$output" 2>&1 || status=$?
    return "$status"
}

run_case() {
    local scenario=$1 expected_status=$2 expected_text=$3 status=0
    shift 3
    local output="$WORK/$scenario.out"
    FAKE_SCENARIO=$scenario
    run_gate "$output" "$WORK/$scenario.log" \
        --context fake-disposable \
        --designated-operator-secret fake-designated-operator \
        --timeout-seconds 1 "$@" || status=$?
    FAKE_SCENARIO=success
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
PREREQS_DIR="$(cd "$KUEUE_DIR/../helm/djinn-prereqs" && pwd)"
# The prerequisite must arrive as the PINNED CHART. The gate used to
# `kubectl apply -f deploy/kueue/vendor/kueue-v0.10.0.yaml`; that fork is
# retired, and asserting on a path no consumer installs is the defect this
# contract now guards against rather than reproduces.
grep -Fq -- "upgrade --install djinn-prereqs $PREREQS_DIR" "$SUCCESS_LOG" || fail 'success did not install the pinned Kueue prerequisite chart'
grep -Fq -- '--namespace kueue-system' "$SUCCESS_LOG" || fail 'prerequisite was not installed into its own namespace'
if grep -Eq -- '(^|[[:space:]])apply -f ' "$SUCCESS_LOG"; then
    fail 'gate still applies a static Kueue manifest; the byte-vendored fork is retired'
fi
grep -Fq -- "upgrade --install djinn-inert-kueue-gate $CHART_DIR" "$SUCCESS_LOG" || fail 'success did not install inert Djinn chart'
grep -Fq -- 'kueue.enabled=true' "$SUCCESS_LOG" || fail 'success did not request the Kueue queue topology, so it proved nothing about it'
grep -Fq -- 'migration.designatedOperatorSecret=fake-designated-operator' "$SUCCESS_LOG" || fail 'success did not provide the fresh-install designated operator Secret'
grep -Fq -- "apply -n djinn -f $SCRIPT_DIR/fixtures/precutover-task-run.yaml" "$SUCCESS_LOG" || fail 'success did not apply pre-cutover fixture'
grep -Fq -- 'get workloads -n djinn' "$SUCCESS_LOG" || fail 'success did not query namespace Workloads'

# --- namespace adoption -----------------------------------------------------
# The designated-operator Secret must exist inside `djinn` before the install
# (the bootstrap initContainer resolves it through secretKeyRef), and Helm
# refuses to adopt a namespace it did not create. On a fresh cluster those two
# facts deadlock; the gate has to break the deadlock itself.
grep -Fq -- 'create namespace djinn' "$SUCCESS_LOG" || fail 'gate never creates the namespace the designated-operator Secret has to live in'
grep -Fq -- 'label namespace djinn app.kubernetes.io/managed-by=Helm --overwrite' "$SUCCESS_LOG" || fail 'gate does not stamp the Helm ownership label, so a pre-created namespace fails adoption'
grep -Fq -- 'meta.helm.sh/release-name=djinn-inert-kueue-gate' "$SUCCESS_LOG" || fail 'gate does not stamp the Helm release-name annotation'
grep -Fq -- 'meta.helm.sh/release-namespace=djinn' "$SUCCESS_LOG" || fail 'gate does not stamp the Helm release-namespace annotation'
grep -Fq -- 'get secret fake-designated-operator -n djinn' "$SUCCESS_LOG" || fail 'gate does not verify the designated-operator Secret before a long install wait'
# Adoption must not degrade into namespace.create=false: the chart-rendered
# Namespace object is exactly what the inertness assertion reads.
if grep -Fq -- 'namespace.create=false' "$SUCCESS_LOG"; then
    fail 'gate sidesteps adoption by disabling the chart Namespace; the inertness assertion would then read an object the chart never produced'
fi
# The gate must never write the capture label itself. Reading it back with
# jsonpath is the assertion; writing it would arm the namespace.
if grep -E -- ' (label|annotate|patch) namespace ' "$SUCCESS_LOG" | grep -q 'kueue-managed'; then
    fail 'gate writes the djinn.io/kueue-managed label; a prerequisite-release gate must never arm the namespace'
fi

# --- independent budgets ----------------------------------------------------
# One 120s value used to govern both the whole-stack `--wait` and the fixture
# Pod wait, which is not enough for a fresh Djinn install on any cold cluster.
grep -Fq -- '--timeout 900s' "$SUCCESS_LOG" || fail 'chart installs did not use the default install budget'
if grep -Fq -- '--timeout 1s' "$SUCCESS_LOG"; then
    fail '--timeout-seconds still governs a chart install; the install and fixture waits must have independent budgets'
fi
[ "$(grep -c -- '--timeout 900s' "$SUCCESS_LOG")" -eq 2 ] || fail 'both the prerequisite and the Djinn install must take the install budget'

# --- adopting a namespace that already exists -------------------------------
NS_EXISTS_OUT="$WORK/ns-exists.out"
NS_EXISTS_STATUS=0
FAKE_NAMESPACE_EXISTS=1
run_gate "$NS_EXISTS_OUT" "$WORK/ns-exists.log" \
    --context fake-disposable --designated-operator-secret fake-designated-operator --timeout-seconds 1 || NS_EXISTS_STATUS=$?
FAKE_NAMESPACE_EXISTS=
[ "$NS_EXISTS_STATUS" -eq 0 ] || { cat "$NS_EXISTS_OUT" >&2; fail 'gate could not adopt an already-existing djinn namespace'; }
grep -Fq -- 'already exists; adopting it into release djinn-inert-kueue-gate' "$NS_EXISTS_OUT" || { cat "$NS_EXISTS_OUT" >&2; fail 'pre-existing namespace adoption was not reported'; }
grep -Fq -- 'label namespace djinn app.kubernetes.io/managed-by=Helm --overwrite' "$WORK/ns-exists.log" || fail 'pre-existing namespace was not stamped for adoption'

# --- values passthrough -----------------------------------------------------
# Without this the gate installs the chart's committed multi-node defaults:
# ReadWriteMany claims nothing single-node binds, and an unqualified
# `djinn-server:latest` that resolves to docker.io and 401s.
VALUES_FILE="$WORK/operator-values.yaml"
cat >"$VALUES_FILE" <<'EOF'
storage:
  mirrors:
    accessMode: ReadWriteOnce
EOF
VALUES_OUT="$WORK/values.out"
VALUES_STATUS=0
run_gate "$VALUES_OUT" "$WORK/values.log" \
    --context fake-disposable --designated-operator-secret fake-designated-operator --timeout-seconds 1 \
    --values "$VALUES_FILE" \
    --set image.server=registry.example/djinn-server:9.9.9 \
    --set-string storage.cache.accessMode=ReadWriteOnce || VALUES_STATUS=$?
[ "$VALUES_STATUS" -eq 0 ] || { cat "$VALUES_OUT" >&2; fail 'gate rejected an operator values passthrough'; }
VALUES_LOG="$WORK/values.log"
DJINN_INSTALL_LINE="$(grep -F -- 'upgrade --install djinn-inert-kueue-gate' "$VALUES_LOG")"
[ -n "$DJINN_INSTALL_LINE" ] || fail 'no Djinn chart install recorded for the passthrough case'
printf '%s\n' "$DJINN_INSTALL_LINE" | grep -Fq -- "--values $VALUES_FILE" || fail '--values was not forwarded to the Djinn chart install'
printf '%s\n' "$DJINN_INSTALL_LINE" | grep -Fq -- '--set image.server=registry.example/djinn-server:9.9.9' || fail '--set was not forwarded to the Djinn chart install'
printf '%s\n' "$DJINN_INSTALL_LINE" | grep -Fq -- '--set-string storage.cache.accessMode=ReadWriteOnce' || fail '--set-string was not forwarded to the Djinn chart install'
# Helm applies --set over --values regardless of CLI order, and later --set over
# earlier --set. The gate-owned kueue.enabled must therefore be emitted last, or
# a caller could turn the queue topology off and make "zero Workloads" vacuous.
printf '%s\n' "$DJINN_INSTALL_LINE" | grep -Eq -- '--set image\.server=[^ ]+ .*--set kueue\.enabled=true' || fail 'operator --set is not ordered before the gate-owned --set kueue.enabled=true, so a caller could disable the queue topology'
# The Kueue prerequisite chart is Djinn-independent; Djinn values must not leak.
PREREQS_INSTALL_LINE="$(grep -F -- 'upgrade --install djinn-prereqs' "$VALUES_LOG")"
if printf '%s\n' "$PREREQS_INSTALL_LINE" | grep -Fq -- "--values $VALUES_FILE"; then
    fail 'operator Djinn values leaked into the Kueue prerequisite install'
fi

# --- gate-owned keys are not caller-overridable -----------------------------
reject_case() {
    local name=$1 expected=$2 status=0
    shift 2
    local output="$WORK/$name.out"
    run_gate "$output" "$WORK/$name.log" \
        --context fake-disposable --designated-operator-secret fake-designated-operator "$@" || status=$?
    [ "$status" -ne 0 ] || { cat "$output" >&2; fail "$name unexpectedly passed"; }
    grep -Fq -- "$expected" "$output" || { cat "$output" >&2; fail "$name lacked diagnostic: $expected"; }
}

reject_case override-kueue 'overrides a gate-owned key (kueue.enabled)' --set kueue.enabled=false
reject_case override-kueue-armed 'overrides a gate-owned key (kueue.armed)' --set-string kueue.armed=true
reject_case override-operator-secret 'overrides a gate-owned key (migration.designatedOperatorSecret)' --set migration.designatedOperatorSecret=someone-else
reject_case missing-values-file '--values file does not exist' --values "$WORK/no-such-values.yaml"

# --- an install failure must fail the gate ----------------------------------
# The install path is the only environmentally fallible part of this harness. A
# stub that can only exit 0 leaves it with zero coverage in either direction.
helm_failure_case() {
    local name=$1 status=0
    local output="$WORK/$name.out"
    FAKE_HELM_FAIL_MATCH=$2
    run_gate "$output" "$WORK/$name.log" \
        --context fake-disposable --designated-operator-secret fake-designated-operator --timeout-seconds 1 || status=$?
    FAKE_HELM_FAIL_MATCH=
    [ "$status" -ne 0 ] || { cat "$output" >&2; fail "$name unexpectedly passed despite a failing helm install"; }
    grep -Fq -- 'helm error while installing into context fake-disposable' "$output" || { cat "$output" >&2; fail "$name did not surface the helm failure"; }
    if grep -Fq -- 'PASS: zero-capture prerequisite gate completed' "$output"; then
        fail "$name reported a gate pass after a failed install"
    fi
}

helm_failure_case prereqs-install-failure djinn-prereqs
helm_failure_case chart-install-failure djinn-inert-kueue-gate

# --- designated-operator Secret absent from the namespace -------------------
MISSING_IN_NS_OUT="$WORK/missing-secret-in-ns.out"
MISSING_IN_NS_STATUS=0
FAKE_SECRET_MISSING=1
run_gate "$MISSING_IN_NS_OUT" "$WORK/missing-secret-in-ns.log" \
    --context fake-disposable --designated-operator-secret fake-designated-operator --timeout-seconds 1 || MISSING_IN_NS_STATUS=$?
FAKE_SECRET_MISSING=
[ "$MISSING_IN_NS_STATUS" -ne 0 ] || { cat "$MISSING_IN_NS_OUT" >&2; fail 'gate installed the chart with no designated-operator Secret in the namespace'; }
grep -Fq -- 'is absent from namespace djinn' "$MISSING_IN_NS_OUT" || { cat "$MISSING_IN_NS_OUT" >&2; fail 'missing-Secret failure did not name the namespace'; }
grep -Fq -- 'create secret generic fake-designated-operator' "$MISSING_IN_NS_OUT" || { cat "$MISSING_IN_NS_OUT" >&2; fail 'missing-Secret failure did not print an executable recipe'; }
if grep -Fq -- 'upgrade --install djinn-inert-kueue-gate' "$WORK/missing-secret-in-ns.log"; then
    fail 'gate proceeded to the Djinn install without the designated-operator Secret'
fi

run_case managed-namespace fail 'namespace djinn is labelled djinn.io/kueue-managed=true'
run_case pod-pending fail 'fixture Pod did not reach Running within 1s (last phase: Pending)'
run_case workload-captured fail 'Kueue captured a Workload in namespace djinn'
run_case api-error fail 'kubectl API error while running: kubectl --context fake-disposable get workloads -n djinn'

MISSING_SECRET_OUTPUT="$WORK/missing-secret.out"
MISSING_SECRET_STATUS=0
KUBECTL="$WORK/kubectl" HELM="$WORK/helm" bash "$GATE" --context fake-disposable >"$MISSING_SECRET_OUTPUT" 2>&1 || MISSING_SECRET_STATUS=$?
[ "$MISSING_SECRET_STATUS" -ne 0 ] || fail 'gate unexpectedly accepted a fresh install without a designated operator Secret'
grep -Fq -- 'a caller-provided --designated-operator-secret is required for a fresh chart install' "$MISSING_SECRET_OUTPUT" || { cat "$MISSING_SECRET_OUTPUT" >&2; fail 'missing Secret rejection lacked diagnostic'; }

printf 'PASS: zero-capture gate fake-kubectl contract completed\n'
cat <<'EOF'
UNVERIFIED: the install path of deploy/kueue/zero-capture-gate.sh was NOT
exercised by this run. `helm` and `kubectl` were stubs; no chart was rendered or
installed, no StorageClass was consulted, no image was pulled and no cluster was
contacted. This contract covers argument construction, argument ordering and
failure handling only. It is NOT evidence that the gate can execute against any
cluster, and it is NOT the release-before-cutover evidence epic 4c9q requires.
That evidence comes only from the target-cluster invocation documented in
deploy/runbooks/kueue-inert-release-zero-capture.md.
EOF
