#!/usr/bin/env bash
# Structural contract for Djinn's Kueue admission scoping.
#
# This validates the RENDERED output of deploy/helm/djinn-prereqs — the artifact
# a cluster actually receives. It used to validate
# deploy/kueue/vendor/kueue-v0.10.0.yaml, a byte-vendored fork. That file is
# gone; a test hard-wired to a static path that is not the deployment artifact
# stays green while proving nothing about the cluster.
#
# Every negative case below is a REAL helm render of the same pinned subchart
# with the scoping deliberately broken, not a hand-written fixture. A fixture
# can drift into agreeing with a checker that no longer means anything; a
# mis-scoped render cannot.
#
# Needs `helm` and python3 with PyYAML. Both are present in the CI lane that
# hosts this suite (it also runs `helm lint` and the chart contracts). A missing
# tool FAILS here — it is never skipped.
#
# Usage: bash deploy/kueue/tests/webhook-selectors.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KUEUE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$KUEUE_DIR/../.." && pwd)"
CHART="$REPO_ROOT/deploy/helm/djinn-prereqs"
SELECTOR_CHECKER="$SCRIPT_DIR/check-webhook-selectors.py"
DRIFT_CHECKER="$SCRIPT_DIR/check-manager-config-drift.py"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "required tool '$1' is not installed"
}

require_file() {
    [ -f "$1" ] || fail "required file is missing: $1"
}

require_tool helm
require_tool python3
python3 -c 'import yaml' 2>/dev/null || fail 'python3 PyYAML is required to parse helm output'

require_file "$SELECTOR_CHECKER"
require_file "$DRIFT_CHECKER"
[ -d "$CHART" ] || fail "prerequisite chart is missing: $CHART"

# The pinned dependency must be vendored, not resolved at test time: this suite
# has to render identically with no registry access at all.
DEP_TARBALL=$(find "$CHART/charts" -maxdepth 1 -name 'kueue-*.tgz' 2>/dev/null | head -n1)
[ -n "$DEP_TARBALL" ] || fail "no vendored kueue dependency tarball under $CHART/charts (run: helm dependency update $CHART)"
require_file "$CHART/Chart.lock"

# A retired asset must stay retired; otherwise a stale copy silently becomes
# the thing an operator applies again.
if [ -e "$KUEUE_DIR/vendor" ]; then
    fail "deploy/kueue/vendor still exists; the byte-vendored fork was retired in favour of $CHART"
fi

render_prereqs() {
    local output=$1
    shift
    helm template djinn-prereqs "$CHART" --namespace kueue-system "$@" >"$output"
}

# The availability assertion is only meaningful against the labels the `djinn`
# chart REALLY puts on its Namespace. Extract them from a live render rather
# than hard-coding a guess that could drift away from the chart.
#
# TWO renders, because the chart now has two materially different postures and
# each answers a different question:
#
#   DISARMED (`kueue.armed=false`) — does the PREREQUISITE RELEASE, on its own,
#     bring the djinn namespace into Kueue's webhook scope? It must not. This is
#     the property `djinn-prereqs` is responsible for, and the only one it can
#     be held to: it ships a positive `managedJobsNamespaceSelector` keyed on
#     `djinn.io/kueue-managed`, so scope is decided by whoever applies that
#     label, not by this chart.
#
#   ARMED (chart defaults) — what does arming COST? `kueue.armed: true` now
#     ships as the default, so the djinn namespace carries the label and IS in
#     scope. That is a deliberate, accepted coupling and it is asserted below,
#     by name, rather than left to be discovered during an outage.
#
# Both are real renders. The armed set used to be synthesised here by editing
# the disarmed one in python; reading it off the actual default render is
# strictly stronger, because a chart that stopped applying the label would now
# fail the armed case instead of quietly agreeing with a hand-built fixture.
DJINN_CHART="$REPO_ROOT/deploy/helm/djinn"
helm template djinn "$DJINN_CHART" --is-upgrade --set kueue.armed=false >"$WORK/djinn-disarmed.yaml"
helm template djinn "$DJINN_CHART" --is-upgrade >"$WORK/djinn.yaml"
extract_ns_labels() {
    python3 - "$1" <<'PY'
import json
import sys

import yaml

docs = [d for d in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if d]
namespaces = [d for d in docs if d.get("kind") == "Namespace"]
if len(namespaces) != 1:
    raise SystemExit(f"expected exactly one rendered Namespace, got {len(namespaces)}")
metadata = namespaces[0].get("metadata", {})
labels = metadata.get("labels") or {}
# The API server always synthesises this one, so a selector keyed on it (which
# is exactly what upstream ships) must be evaluated against it too.
labels.setdefault("kubernetes.io/metadata.name", metadata["name"])
print(json.dumps(labels, sort_keys=True))
PY
}
DJINN_NS_LABELS=$(extract_ns_labels "$WORK/djinn-disarmed.yaml") ||
    fail 'could not extract the djinn Namespace labels from a disarmed chart render'
DJINN_NS_LABELS_ARMED=$(extract_ns_labels "$WORK/djinn.yaml") ||
    fail 'could not extract the djinn Namespace labels from the default chart render'
printf 'INFO: evaluating webhook namespaceSelectors against real djinn Namespace labels\n'
printf 'INFO:   disarmed (kueue.armed=false): %s\n' "$DJINN_NS_LABELS"
printf 'INFO:   armed    (chart defaults):    %s\n' "$DJINN_NS_LABELS_ARMED"

# Non-vacuity for the extraction itself: an empty label set would make the
# selector evaluation trivially non-matching and quietly prove nothing.
python3 -c 'import json,sys; d=json.loads(sys.argv[1]); sys.exit(0 if d.get("kubernetes.io/metadata.name") else 1)' "$DJINN_NS_LABELS" \
    || fail 'extracted djinn Namespace labels lack kubernetes.io/metadata.name; the selector evaluation would be vacuous'

# The two renders must actually DIFFER on the fence label, or every case below
# collapses into the same question asked twice.
python3 - "$DJINN_NS_LABELS" "$DJINN_NS_LABELS_ARMED" <<'PY' || fail 'the armed and disarmed renders do not differ on djinn.io/kueue-managed; the cases below would be redundant'
import json
import sys

disarmed, armed = (json.loads(arg) for arg in sys.argv[1:])
assert "djinn.io/kueue-managed" not in disarmed, (
    f"kueue.armed=false still labelled the namespace: {disarmed}"
)
assert armed.get("djinn.io/kueue-managed") == "true", (
    "the chart's DEFAULT render must label the namespace djinn.io/kueue-managed=true "
    f"(kueue.armed ships true); got {armed}"
)
PY

check_render() {
    python3 "$SELECTOR_CHECKER" "$1" \
        --namespace-name djinn --namespace-labels "${2:-$DJINN_NS_LABELS}"
}

expect_pass() {
    local label=$1 manifest=$2
    if ! check_render "$manifest"; then
        fail "$label: the shipped render must satisfy the scoping contract"
    fi
}

# A negative must fail FOR THE STATED REASON. A render that errors out, or that
# fails on an unrelated assertion, would otherwise be mistaken for proof.
expect_rejected() {
    local label=$1 manifest=$2
    shift 2
    local output status
    set +e
    output=$(check_render "$manifest" 2>&1)
    status=$?
    set -e
    [ "$status" -eq 1 ] || {
        printf '%s\n' "$output" >&2
        fail "$label: expected the checker to reject (exit 1), got exit $status"
    }
    local expected
    for expected in "$@"; do
        grep -Fq -- "$expected" <<<"$output" || {
            printf '%s\n' "$output" >&2
            fail "$label: rejected, but not for the expected reason: $expected"
        }
    done
    printf 'PASS: checker rejected %s\n' "$label"
}

echo "=== shipped render: deploy/helm/djinn-prereqs brings nothing into scope on its own ==="
# Evaluated against the DISARMED djinn namespace. This is the prerequisite
# release's own contract: installing it must not put any djinn workload behind a
# Kueue webhook. Scope is granted by the `djinn.io/kueue-managed` label, which
# the `djinn` chart applies only when armed — see the armed case below.
render_prereqs "$WORK/shipped.yaml"
expect_pass 'shipped render' "$WORK/shipped.yaml"

echo "=== drift: manager config vs the pinned subchart's own default ==="
python3 "$DRIFT_CHECKER" "$CHART" || fail 'manager config drifted from the pinned upstream default'

echo "=== negative: the SAME pinned subchart rendered with UPSTREAM defaults ==="
# This is the whole point of the contract. If Djinn's values stopped being
# applied — a renamed key, a lost values.yaml, a subchart alias change — the
# render would collapse to exactly this, and it must be rejected.
helm template kueue "$DEP_TARBALL" --namespace kueue-system >"$WORK/upstream-default.yaml"
expect_rejected 'upstream-default render' "$WORK/upstream-default.yaml" \
    'namespaceSelector must be exactly' \
    "failurePolicy must be 'Ignore' for ['pods']" \
    "webhook mpod.kb.io: namespaceSelector SELECTS namespace 'djinn'" \
    "webhook vjob.kb.io: namespaceSelector SELECTS namespace 'djinn'"

echo "=== WHAT THE ARMED DEFAULT COSTS: the djinn namespace IS in Kueue's webhook scope ==="
# THIS IS NOT A FAILURE. It is the accepted price of `kueue.armed: true`, pinned
# here so it stays a named, deliberate coupling instead of an outage nobody
# predicted. Kueue admission cannot exist without the webhooks that implement
# it, so arming necessarily puts the djinn namespace behind them.
#
# What that buys: Kueue's pods quota, which is the ONLY remaining
# build-concurrency bound (the in-process reservation authority was deleted in
# #2822). An unarmed install has no bound at all.
#
# What it costs, precisely: `vjob.kb.io` carries `failurePolicy: Fail`, so while
# the Kueue controller is UNAVAILABLE, Job CREATE in the djinn namespace is
# rejected — i.e. task-run, warm and SCIP dispatch all stop until it recovers.
# The Pod/Deployment/StatefulSet webhooks are `failurePolicy: Ignore` and
# degrade instead of blocking, which is why djinn-server, Postgres and Qdrant
# keep being schedulable through the same outage. Install the prerequisite with
# `--wait`, and treat Kueue controller availability as a dispatch dependency.
#
# The assertion is on the checker's REPORT, not on a verdict: the same shipped
# prereqs render that passed above is reported as in-scope purely by swapping in
# the armed namespace labels, which is also what keeps the selector evaluation
# non-vacuous in both directions.
set +e
LABELLED_OUT=$(check_render "$WORK/shipped.yaml" "$DJINN_NS_LABELS_ARMED" 2>&1)
LABELLED_STATUS=$?
set -e
[ "$LABELLED_STATUS" -eq 1 ] || {
    printf '%s\n' "$LABELLED_OUT" >&2
    fail 'the armed default was not reported as bringing the djinn namespace into Kueue scope; either the chart stopped labelling the namespace or the checker stopped evaluating selectors'
}
grep -Fq -- "webhook vjob.kb.io: namespaceSelector SELECTS namespace 'djinn'" <<<"$LABELLED_OUT" || {
    printf '%s\n' "$LABELLED_OUT" >&2
    fail 'expected the Job webhook to be reported as selecting the armed djinn namespace'
}
grep -Fq -- "failurePolicy 'Fail'" <<<"$LABELLED_OUT" || {
    printf '%s\n' "$LABELLED_OUT" >&2
    fail 'the report no longer names the failurePolicy that makes this a dispatch dependency'
}
printf 'PASS: the armed default is reported as bringing Job CREATE into a failurePolicy=Fail webhook — accepted, and named\n'

echo "=== negative: 'pod' put back into integrations.frameworks ==="
# Isolates the frameworks half: the namespace fence is left intact, so the only
# possible rejection is the failurePolicy one.
python3 - "$CHART/values.yaml" "$WORK/pod-framework.yaml" <<'PY'
import sys
import yaml

source, target = sys.argv[1:]
values = yaml.safe_load(open(source, encoding="utf-8"))
raw = values["kueue"]["managerConfig"]["controllerManagerConfigYaml"]
config = yaml.safe_load(raw)
config["integrations"]["frameworks"].append("pod")
open(target, "w", encoding="utf-8").write(yaml.safe_dump(config, sort_keys=False))
PY
render_prereqs "$WORK/pod-armed.yaml" \
    --set-file "kueue.managerConfig.controllerManagerConfigYaml=$WORK/pod-framework.yaml"
expect_rejected 'pod re-added to frameworks' "$WORK/pod-armed.yaml" \
    "failurePolicy must be 'Ignore' for ['pods']"

echo "=== negative: namespace fence dropped, frameworks left correct ==="
# Isolates the other half.
python3 - "$CHART/values.yaml" "$WORK/no-fence.yaml" <<'PY'
import sys
import yaml

source, target = sys.argv[1:]
values = yaml.safe_load(open(source, encoding="utf-8"))
raw = values["kueue"]["managerConfig"]["controllerManagerConfigYaml"]
config = yaml.safe_load(raw)
del config["managedJobsNamespaceSelector"]
open(target, "w", encoding="utf-8").write(yaml.safe_dump(config, sort_keys=False))
PY
render_prereqs "$WORK/unfenced.yaml" \
    --set-file "kueue.managerConfig.controllerManagerConfigYaml=$WORK/no-fence.yaml"
expect_rejected 'managedJobsNamespaceSelector removed' "$WORK/unfenced.yaml" \
    'namespaceSelector must be exactly'

echo "=== negative: a checker fed a manifest with no webhooks must NOT pass ==="
# kueue.enabled=false is a legitimate configuration, but it must not be able to
# masquerade as a passing scoping proof. The checker has to say it asserted
# nothing rather than return success on an empty roster.
render_prereqs "$WORK/disabled.yaml" --set kueue.enabled=false
grep -q 'WebhookConfiguration' "$WORK/disabled.yaml" && \
    fail 'kueue.enabled=false still rendered admission webhooks; the dependency condition is not wired'
expect_rejected 'kueue.enabled=false render' "$WORK/disabled.yaml" \
    'no admission webhook configurations found'

echo "=== negative: drift checker rejects an unsanctioned manager-config edit ==="
DRIFT_CHART="$WORK/drift-chart"
cp -R "$CHART" "$DRIFT_CHART"
python3 - "$DRIFT_CHART/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
raw = values["kueue"]["managerConfig"]["controllerManagerConfigYaml"]
config = yaml.safe_load(raw)
# An edit outside the two sanctioned ones. This is exactly the class of change
# that a wholesale-restated upstream default lets in unnoticed.
config["clientConnection"]["qps"] = 1
values["kueue"]["managerConfig"]["controllerManagerConfigYaml"] = yaml.safe_dump(
    config, sort_keys=False
)
open(path, "w", encoding="utf-8").write(yaml.safe_dump(values, sort_keys=False))
PY
if python3 "$DRIFT_CHECKER" "$DRIFT_CHART" >"$WORK/drift.out" 2>&1; then
    cat "$WORK/drift.out" >&2
    fail 'drift checker accepted an unsanctioned manager-config edit'
fi
grep -Fq 'drifted from upstream outside the two' "$WORK/drift.out" || {
    cat "$WORK/drift.out" >&2
    fail 'drift checker rejected for the wrong reason'
}
printf 'PASS: drift checker rejected an unsanctioned manager-config edit\n'

echo "=== scope reduction on record ==="
cat <<'EOF'
NOTE 1: mpod/vpod (and the deployment/statefulset pairs) CANNOT be unregistered
        through chart values. Upstream renders them unconditionally and uses
        integrations.frameworks only to switch failurePolicy. Removing "pod",
        "deployment" and "statefulset" gives failurePolicy: Ignore, which is
        asserted above and is the real availability guarantee: an unavailable
        Kueue controller is skipped, not fatal, for those creations.
NOTE 2: objectSelector (djinn.io/kueue-build-object) is GONE, not weakened. The
        upstream chart exposes no hook for a per-object fence at any version.
NOTE 3: mjob/vjob keep failurePolicy: Fail and are NAMESPACE-fenced only. In a
        namespace labelled djinn.io/kueue-managed=true a Kueue outage would
        block every batch/v1 Job CREATE in it. Nothing in this repository
        applies that label — asserted above against the djinn chart's REAL
        rendered Namespace labels, so today's blast radius is zero. Input to
        cutover epic 4c9q; see deploy/kueue/README.md.
EOF

printf 'PASS: Kueue admission scoping contract completed\n'
