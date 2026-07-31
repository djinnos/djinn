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
DJINN_CHART="$REPO_ROOT/deploy/helm/djinn"
helm template djinn "$DJINN_CHART" --is-upgrade >"$WORK/djinn.yaml"
DJINN_NS_LABELS=$(python3 - "$WORK/djinn.yaml" <<'PY'
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
) || fail 'could not extract the djinn Namespace labels from a chart render'
printf 'INFO: evaluating webhook namespaceSelectors against real djinn Namespace labels: %s\n' "$DJINN_NS_LABELS"

# Non-vacuity for the extraction itself: an empty label set would make the
# selector evaluation trivially non-matching and quietly prove nothing.
python3 -c 'import json,sys; d=json.loads(sys.argv[1]); sys.exit(0 if d.get("kubernetes.io/metadata.name") else 1)' "$DJINN_NS_LABELS" \
    || fail 'extracted djinn Namespace labels lack kubernetes.io/metadata.name; the selector evaluation would be vacuous'

check_render() {
    python3 "$SELECTOR_CHECKER" "$1" \
        --namespace-name djinn --namespace-labels "$DJINN_NS_LABELS"
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

echo "=== shipped render: deploy/helm/djinn-prereqs ==="
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

echo "=== negative: what labelling the djinn namespace would cost (4c9q input) ==="
# The fence is correct; the namespace is not. This is the 4c9q decision made the
# wrong way, and it must be visible as a failure here rather than discovered in
# production. It also proves the selector evaluation is non-vacuous: the same
# shipped render that passed above is rejected purely by changing the labels.
LABELLED_NS=$(python3 -c 'import json,sys; d=json.loads(sys.argv[1]); d["djinn.io/kueue-managed"]="true"; print(json.dumps(d, sort_keys=True))' "$DJINN_NS_LABELS")
set +e
LABELLED_OUT=$(python3 "$SELECTOR_CHECKER" "$WORK/shipped.yaml" --namespace-name djinn --namespace-labels "$LABELLED_NS" 2>&1)
LABELLED_STATUS=$?
set -e
[ "$LABELLED_STATUS" -eq 1 ] || {
    printf '%s\n' "$LABELLED_OUT" >&2
    fail 'labelling the djinn namespace was not reported as bringing it into Kueue scope'
}
grep -Fq -- "webhook vjob.kb.io: namespaceSelector SELECTS namespace 'djinn'" <<<"$LABELLED_OUT" || {
    printf '%s\n' "$LABELLED_OUT" >&2
    fail 'expected the Job webhook to be reported as selecting a labelled djinn namespace'
}
printf 'PASS: labelling djinn.io/kueue-managed on the djinn namespace is reported as bringing Job CREATE into a failurePolicy=Fail webhook\n'

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
