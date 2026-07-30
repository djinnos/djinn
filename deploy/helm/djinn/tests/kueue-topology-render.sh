#!/usr/bin/env bash
# Structural Helm render contract for the Kueue topology and its arming switch.
#
# TWO FLAGS, NOT ONE:
#   kueue.enabled — renders the ResourceFlavor/ClusterQueue/LocalQueue topology.
#   kueue.armed   — labels the Namespace djinn.io/kueue-managed AND sets
#                   DJINN_KUEUE_ARMED on djinn-server, which is what makes the
#                   Job renderers emit `suspend: true` + a queue-name label.
#
# The pair exists so "Kueue installed, topology rendered, nothing captured" stays
# representable — the state deploy/kueue/zero-capture-gate.sh verifies. Collapsing
# them would make it unrepresentable and leave that gate with nothing to check.
#
# Usage: bash deploy/helm/djinn/tests/kueue-topology-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$CHART_DIR/../../.." && pwd)"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    }
}

require_tool helm
require_tool python3
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

render() {
    local output=$1
    shift
    helm template kueue-topology-test "$CHART_DIR" --is-upgrade "$@" >"$output"
}

# Same render, with helm's stderr folded into the output file. Rejection cases
# grep the diagnostic, and `fail` messages arrive on stderr.
render_capture() {
    local output=$1
    shift
    helm template kueue-topology-test "$CHART_DIR" --is-upgrade "$@" >"$output" 2>&1
}

# The topology is `kueue.x-k8s.io/v1beta1`, so it only renders when the operator
# has declared the Kueue prerequisite installed. Everything below that asserts
# topology shape therefore has to opt in explicitly.
render_enabled() {
    local output=$1
    shift
    render "$output" --set kueue.enabled=true "$@"
}

# `expected_managed` is yes/no: whether the Namespace must carry
# djinn.io/kueue-managed=true. It is the arming half of the contract, asserted in
# BOTH directions so neither branch can rot into a tautology.
assert_topology() {
    local manifest=$1 expected_pods=$2 expected_managed=$3
    python3 - "$manifest" "$expected_pods" "$expected_managed" <<'PY'
import sys
import yaml

manifest, expected_pods, expected_managed = sys.argv[1:]
docs = [doc for doc in yaml.safe_load_all(open(manifest, encoding="utf-8")) if doc]

def named(kind):
    return [doc for doc in docs if doc.get("kind") == kind]

flavors = named("ResourceFlavor")
assert len(flavors) == 1, f"expected one ResourceFlavor, got {len(flavors)}"
flavor = flavors[0]
assert flavor.get("spec") == {}, "ResourceFlavor must be empty"

queues = named("ClusterQueue")
assert len(queues) == 1, f"expected one ClusterQueue, got {len(queues)}"
queue = queues[0]
spec = queue["spec"]
# BestEffortFIFO, not StrictFIFO: three kinds share a 3-slot queue, so one
# head-of-line Workload that cannot fit would block everything behind it.
assert spec.get("queueingStrategy") == "BestEffortFIFO", (
    f"ClusterQueue must use BestEffortFIFO, got {spec.get('queueingStrategy')}"
)
groups = spec.get("resourceGroups")
assert isinstance(groups, list) and len(groups) == 1, "ClusterQueue must have one resource group"
group = groups[0]
assert group.get("coveredResources") == ["pods"], "only pods may be covered"
flavor_quotas = group.get("flavors")
assert isinstance(flavor_quotas, list) and len(flavor_quotas) == 1, "exactly one flavor quota is required"
assert flavor_quotas[0].get("name") == flavor["metadata"]["name"], "quota must use the empty flavor"
resources = flavor_quotas[0].get("resources")
assert isinstance(resources, list) and len(resources) == 1, "exactly one nominal quota is required"
quota = resources[0]
assert quota.get("name") == "pods", "sole nominal quota must be pods"
assert quota.get("nominalQuota") == int(expected_pods), "pods quota must render buildPods verbatim"
assert "cpu" not in str(spec).lower() and "memory" not in str(spec).lower(), "CPU/memory quota is forbidden"

# Exactly three LocalQueues. SCIP joins Kueue because rust-analyzer indexing is
# CPU-heavy and excluding it under-counts the load the quota exists to bound.
# Image build stays out on purpose: it is an UPSTREAM DEPENDENCY of a task-run,
# so a shared ClusterQueue admits a priority-inversion deadlock. A fourth queue
# here is a regression, not an addition.
local_queues = named("LocalQueue")
assert len(local_queues) == 3, f"expected task-run, warm and scip LocalQueues, got {len(local_queues)}"
local_names = {local["metadata"]["name"] for local in local_queues}
assert local_names == {
    "kueue-topology-test-djinn-task-run",
    "kueue-topology-test-djinn-warm",
    "kueue-topology-test-djinn-scip",
}, f"unexpected LocalQueue set: {sorted(local_names)}"
assert all(local.get("spec", {}).get("clusterQueue") == queue["metadata"]["name"] for local in local_queues)

namespaces = named("Namespace")
assert len(namespaces) == 1, f"expected one Namespace, got {len(namespaces)}"
labels = namespaces[0].get("metadata", {}).get("labels", {})
managed = labels.get("djinn.io/kueue-managed")
if expected_managed == "yes":
    # Kueue captures Jobs ONLY in a labelled namespace. Without this label the
    # `suspend: true` the same flag turns on is never undone, and every build
    # Job hangs forever.
    assert managed == "true", (
        f"kueue.armed=true must label the Namespace djinn.io/kueue-managed=true, got {managed!r}"
    )
else:
    assert "djinn.io/kueue-managed" not in labels, (
        "Namespace must remain unlabelled for Kueue unless kueue.armed=true"
    )

roles = named("Role")
assert len(roles) >= 1, "expected namespaced controller Role"
workload_rules = [
    (role, rule) for role in roles for rule in role.get("rules", [])
    if rule.get("apiGroups") == ["kueue.x-k8s.io"] and rule.get("resources") == ["workloads"]
]
assert len(workload_rules) == 1, f"expected exactly one Workload RBAC rule, got {len(workload_rules)}"
workload_role, workload_rule = workload_rules[0]
assert workload_role.get("metadata", {}).get("namespace") == "djinn", "Workload RBAC must remain namespaced"
assert workload_rule.get("verbs") == ["get", "list", "watch"], "Workloads must be observation-only"

for doc in docs:
    if doc.get("kind") not in {"Role", "ClusterRole", "RoleBinding", "ClusterRoleBinding"}:
        continue
    for rule in doc.get("rules", []):
        forbidden = {resource.lower().split("/", 1)[0] for resource in rule.get("resources", [])} & {"nodes", "persistentvolumes"}
        assert not forbidden, f"forbidden cluster resource RBAC: {forbidden}"

server_deployment = next(
    deployment for deployment in named("Deployment")
    if deployment.get("metadata", {}).get("name", "").endswith("-server")
)
containers = server_deployment["spec"]["template"]["spec"]["containers"]
server = next(container for container in containers if container["name"] == "djinn-server")
values = {entry["name"]: entry.get("value") for entry in server["env"]}
# Build admission is gone: the pods-quota reservation authority was deleted, so
# the server must receive neither of its env vars. Kueue's pods quota is the
# only remaining build-concurrency bound, and it is deliberately independent.
# See deploy/helm/djinn/tests/build-admission-topology.sh.
assert "DJINN_BUILD_ADMISSION_MODE" not in values, "removed build-admission env var came back"
assert "DJINN_MAX_BUILD_TASKRUNS" not in values, "removed build-admission cap env var came back"
# The renderer half of the arming contract. It must move with the namespace
# label, never independently: they are two halves of one capture decision.
expected_armed = "true" if expected_managed == "yes" else "false"
assert values["DJINN_KUEUE_ARMED"] == expected_armed, (
    f"DJINN_KUEUE_ARMED must be {expected_armed}, got {values.get('DJINN_KUEUE_ARMED')!r}"
)
# An armed Job's queue-name label has to resolve to a LocalQueue this release
# actually rendered, or it is never admitted.
assert values["DJINN_KUEUE_LOCAL_QUEUE_PREFIX"] == "kueue-topology-test-djinn", (
    f"queue prefix must match the rendered LocalQueue names, got {values.get('DJINN_KUEUE_LOCAL_QUEUE_PREFIX')!r}"
)
PY
}

echo "=== zero-capture-representable state: enabled=true, armed=false ==="
# This is the configuration deploy/kueue/zero-capture-gate.sh exists to verify:
# the whole topology present, the Namespace still unlabelled, nothing captured.
render_enabled "$WORK/valid.yaml" --set kueue.buildPods=7
assert_topology "$WORK/valid.yaml" 7 no

echo "=== armed state: enabled=true, armed=true ==="
render_enabled "$WORK/armed.yaml" --set kueue.buildPods=7 --set kueue.armed=true
assert_topology "$WORK/armed.yaml" 7 yes

echo "=== chart default omits the topology entirely ==="
# Regression guard for the defect this flag fixes: the topology used to render
# unconditionally, so `helm install djinn` failed on every cluster without the
# Kueue CRDs (observed on the production VPS). The default must be installable.
render "$WORK/default.yaml"
python3 - "$WORK/default.yaml" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]
topology = [
    doc for doc in docs
    if str(doc.get("apiVersion", "")).startswith("kueue.x-k8s.io/")
]
assert not topology, (
    "chart defaults must render no kueue.x-k8s.io objects, got "
    f"{[(d['kind'], d['metadata']['name']) for d in topology]}"
)
# Non-vacuity: the render must still be a real chart render, not an empty file
# that trivially satisfies the assertion above.
assert any(doc.get("kind") == "Deployment" for doc in docs), (
    "default render produced no Deployment; the assertion above proved nothing"
)
# The controller's observation-only Workload RBAC is deliberately NOT gated:
# it is inert without the CRDs and must not churn with the flag.
rules = [
    rule for doc in docs if doc.get("kind") == "Role"
    for rule in doc.get("rules", [])
    if rule.get("apiGroups") == ["kueue.x-k8s.io"]
]
assert len(rules) == 1, f"expected the Workload RBAC rule to survive, got {rules}"
PY

echo "=== explicitly disabled omits the topology ==="
render "$WORK/disabled.yaml" --set kueue.enabled=false --set kueue.buildPods=7
grep -q 'kueue.x-k8s.io/v1beta1' "$WORK/disabled.yaml" && {
    echo "FAIL: kueue.enabled=false still rendered the v1beta1 topology" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# The topology flag alone arms nothing.
# ---------------------------------------------------------------------------
assert_disarmed_server() {
    local manifest=$1 label=$2
    python3 - "$manifest" "$label" <<'PY'
import sys
import yaml

manifest, label = sys.argv[1:]
docs = [doc for doc in yaml.safe_load_all(open(manifest, encoding="utf-8")) if doc]

namespaces = [doc for doc in docs if doc.get("kind") == "Namespace"]
assert len(namespaces) == 1, f"{label}: expected one Namespace, got {len(namespaces)}"
labels = namespaces[0].get("metadata", {}).get("labels", {})
assert "djinn.io/kueue-managed" not in labels, (
    f"{label}: the topology flag must not label the Namespace"
)

server = next(
    container
    for doc in docs
    if doc.get("kind") == "Deployment"
    and doc.get("metadata", {}).get("name", "").endswith("-server")
    for container in doc["spec"]["template"]["spec"]["containers"]
    if container["name"] == "djinn-server"
)
values = {entry["name"]: entry.get("value") for entry in server["env"]}
assert values["DJINN_KUEUE_ARMED"] == "false", (
    f"{label}: the topology flag must not arm the renderers, got "
    f"{values.get('DJINN_KUEUE_ARMED')!r}"
)
PY
}

# Same assertion for an externally-managed namespace, where there is no
# Namespace object to check at all.
assert_disarmed_server_no_namespace() {
    local manifest=$1 label=$2
    python3 - "$manifest" "$label" <<'PY'
import sys
import yaml

manifest, label = sys.argv[1:]
docs = [doc for doc in yaml.safe_load_all(open(manifest, encoding="utf-8")) if doc]

assert not [doc for doc in docs if doc.get("kind") == "Namespace"], (
    f"{label}: namespace.create=false must render no Namespace object"
)
server = next(
    container
    for doc in docs
    if doc.get("kind") == "Deployment"
    and doc.get("metadata", {}).get("name", "").endswith("-server")
    for container in doc["spec"]["template"]["spec"]["containers"]
    if container["name"] == "djinn-server"
)
values = {entry["name"]: entry.get("value") for entry in server["env"]}
assert values["DJINN_KUEUE_ARMED"] == "false", (
    f"{label}: expected a disarmed server, got {values.get('DJINN_KUEUE_ARMED')!r}"
)
PY
}

echo "=== kueue.enabled alone arms nothing, at BOTH of its values ==="
assert_disarmed_server "$WORK/default.yaml" "kueue.enabled=false"
assert_disarmed_server "$WORK/valid.yaml" "kueue.enabled=true"

# ---------------------------------------------------------------------------
# armed implies enabled
# ---------------------------------------------------------------------------
echo "=== armed=true with enabled=false is rejected by name ==="
if render_capture "$WORK/armed-without-enabled.out" --set kueue.armed=true; then
    echo "FAIL: kueue.armed=true with kueue.enabled=false rendered successfully" >&2
    exit 1
fi
# The invariant must be NAMED, not merely violated by some unrelated error.
grep -q 'kueue.armed=true requires kueue.enabled=true' "$WORK/armed-without-enabled.out" || {
    echo "FAIL: the armed-implies-enabled rejection did not name the invariant:" >&2
    cat "$WORK/armed-without-enabled.out" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# RuntimeClass stacking: mirror of the renderer assert at job.rs:242.
# ---------------------------------------------------------------------------
echo "=== armed=true with a required cgroup launcher and no RuntimeClass is rejected ==="
RUNTIME_CLASS_VALUES=(
    --set kueue.enabled=true
    --set-string cgroupLauncher.mode=required
    --set cgroupWritable.taskRuns.enabled=false
)
if render_capture "$WORK/armed-runtimeclass.out" "${RUNTIME_CLASS_VALUES[@]}" --set kueue.armed=true; then
    echo "FAIL: arming Kueue over an unsatisfiable cgroup launcher rendered successfully" >&2
    exit 1
fi
grep -q 'required cgroup launcher requires runtimeClassName: djinn-cgroup-writable' "$WORK/armed-runtimeclass.out" || {
    echo "FAIL: the RuntimeClass rejection did not mirror the job.rs:242 assertion:" >&2
    cat "$WORK/armed-runtimeclass.out" >&2
    exit 1
}
# Non-vacuity: the SAME values with armed=false must render, so the failure above
# cannot be an unrelated chart error that would reject either way.
render "$WORK/disarmed-runtimeclass.yaml" "${RUNTIME_CLASS_VALUES[@]}" --set kueue.armed=false
assert_topology "$WORK/disarmed-runtimeclass.yaml" 3 no

# ---------------------------------------------------------------------------
# Arming requires a namespace the chart can actually label.
#
# `namespace.create: false` renders no Namespace object, so the chart CANNOT
# apply djinn.io/kueue-managed while still setting DJINN_KUEUE_ARMED. Every
# build Job would render `suspend: true` into a namespace Kueue does not manage,
# nothing would capture them, and nothing would ever unsuspend them. That is the
# exact failure the one-flag design exists to prevent, reachable through a values
# combination the chart used to accept in silence.
# ---------------------------------------------------------------------------
echo "=== armed=true with namespace.create=false is rejected by name ==="
if render_capture "$WORK/armed-external-ns.out" \
    --set kueue.enabled=true --set kueue.armed=true --set namespace.create=false; then
    echo "FAIL: kueue.armed=true with namespace.create=false rendered successfully" >&2
    exit 1
fi
# The hazard must be NAMED, not merely violated by some unrelated render error.
grep -q 'kueue.armed=true with namespace.create=false cannot label the namespace' \
    "$WORK/armed-external-ns.out" || {
    echo "FAIL: the unlabelled-namespace rejection did not name the hazard:" >&2
    cat "$WORK/armed-external-ns.out" >&2
    exit 1
}
grep -q 'djinn.io/kueue-managed' "$WORK/armed-external-ns.out" || {
    echo "FAIL: the rejection did not name the label Kueue actually requires:" >&2
    cat "$WORK/armed-external-ns.out" >&2
    exit 1
}
# The gate lives in templates/namespace.yaml, i.e. in the template that renders
# NOTHING in this configuration. Prove it still fires when helm is asked for one
# unrelated template, because `--show-only` filters after rendering.
if helm template kueue-topology-test "$CHART_DIR" --is-upgrade \
    --set kueue.enabled=true --set kueue.armed=true --set namespace.create=false \
    --show-only templates/deployment-server.yaml >"$WORK/armed-external-ns-showonly.out" 2>&1; then
    echo "FAIL: the unlabelled-namespace gate did not fire under --show-only" >&2
    exit 1
fi
grep -q 'kueue.armed=true with namespace.create=false cannot label the namespace' \
    "$WORK/armed-external-ns-showonly.out" || {
    echo "FAIL: --show-only skipped the gate in templates/namespace.yaml:" >&2
    cat "$WORK/armed-external-ns-showonly.out" >&2
    exit 1
}

echo "=== namespace.create=false alone still renders; only ARMING is gated ==="
# Non-vacuity: an externally-managed namespace is a supported configuration and
# must keep rendering. Without this the rejection above could be an unrelated
# namespace.create=false breakage.
render "$WORK/external-ns-disarmed.yaml" --set kueue.enabled=true --set namespace.create=false
assert_disarmed_server_no_namespace "$WORK/external-ns-disarmed.yaml" "namespace.create=false, armed=false"

echo "=== the acknowledgement reopens the escape hatch ==="
# The gate must not be an unconditional fail: an operator who HAS labelled their
# externally-managed namespace can still arm.
render "$WORK/external-ns-acked.yaml" \
    --set kueue.enabled=true --set kueue.armed=true --set namespace.create=false \
    --set kueue.namespaceLabelledExternally=true
python3 - "$WORK/external-ns-acked.yaml" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]

# No Namespace object: that is the whole premise of this configuration.
assert not [doc for doc in docs if doc.get("kind") == "Namespace"], (
    "namespace.create=false must still render no Namespace object"
)
# The topology must be there for the armed Jobs to be admitted into.
local_queues = [doc for doc in docs if doc.get("kind") == "LocalQueue"]
assert len(local_queues) == 3, f"expected three LocalQueues, got {len(local_queues)}"

server = next(
    container
    for doc in docs
    if doc.get("kind") == "Deployment"
    and doc.get("metadata", {}).get("name", "").endswith("-server")
    for container in doc["spec"]["template"]["spec"]["containers"]
    if container["name"] == "djinn-server"
)
values = {entry["name"]: entry.get("value") for entry in server["env"]}
# The escape hatch must produce a genuinely ARMED render — otherwise the
# acknowledgement would be a no-op that only silenced the gate.
assert values["DJINN_KUEUE_ARMED"] == "true", (
    "the acknowledgement must still arm the renderers, got "
    f"{values.get('DJINN_KUEUE_ARMED')!r}"
)
assert values["DJINN_KUEUE_LOCAL_QUEUE_PREFIX"] == "kueue-topology-test-djinn", (
    f"queue prefix must match the rendered LocalQueues, got {values.get('DJINN_KUEUE_LOCAL_QUEUE_PREFIX')!r}"
)
PY

echo "=== the acknowledgement does NOT arm anything on its own ==="
# It is an acknowledgement, not a second arming switch.
render "$WORK/ack-without-armed.yaml" \
    --set kueue.enabled=true --set kueue.namespaceLabelledExternally=true
assert_topology "$WORK/ack-without-armed.yaml" 3 no

echo "=== the acknowledgement is inert when the chart owns the namespace ==="
# namespace.create=true is unaffected by the new value in either direction.
render "$WORK/ack-with-owned-ns.yaml" \
    --set kueue.enabled=true --set kueue.armed=true --set kueue.namespaceLabelledExternally=true
assert_topology "$WORK/ack-with-owned-ns.yaml" 3 yes

echo "=== the preflight's namespace RBAC is granted ==="
# The render-time acknowledgement is an unverified claim; djinn-server re-checks
# it at startup by GETting its own Namespace. That read needs a namespaced `get`
# on `namespaces` — without it the preflight can only report "unverifiable" and
# disarms, silently costing the deployment its Kueue quota.
python3 - "$WORK/armed.yaml" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]
rules = [
    rule
    for doc in docs
    if doc.get("kind") == "Role"
    for rule in doc.get("rules", [])
    if rule.get("apiGroups") == [""] and "namespaces" in rule.get("resources", [])
]
assert len(rules) == 1, f"expected exactly one namespaces RBAC rule, got {rules}"
# Read-only, and namespaced: the apiserver scopes `get namespaces` in a Role to
# the Role's own namespace, so this grants reading THIS namespace and nothing
# else. Any write verb, or a ClusterRole, would be a real privilege escalation.
assert rules[0].get("verbs") == ["get"], (
    f"the preflight's namespace read must stay get-only, got {rules[0].get('verbs')}"
)
assert not [
    doc for doc in docs
    if doc.get("kind") == "ClusterRole"
    for rule in doc.get("rules", [])
    if "namespaces" in rule.get("resources", [])
], "reading the namespace must NOT require a ClusterRole"
PY

expect_rejected() {
    local name=$1
    shift
    echo "=== invalid kueue.buildPods: $name ==="
    if render_enabled "$WORK/$name.out" "$@" 2>&1; then
        echo "FAIL: invalid Kueue scenario '$name' rendered successfully" >&2
        exit 1
    fi
}

expect_rejected zero --set kueue.buildPods=0
expect_rejected fractional --set kueue.buildPods=1.5
expect_rejected string --set-string kueue.buildPods=seven

echo "=== invalid kueue.enabled ==="
if render "$WORK/enabled-string.out" --set-string kueue.enabled=yes 2>&1; then
    echo "FAIL: a non-boolean kueue.enabled rendered successfully" >&2
    exit 1
fi

echo "=== invalid kueue.armed ==="
if render "$WORK/armed-string.out" --set-string kueue.armed=yes 2>&1; then
    echo "FAIL: a non-boolean kueue.armed rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.enabled ==="
cp -R "$CHART_DIR" "$WORK/chart-without-enabled"
python3 - "$WORK/chart-without-enabled/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values["kueue"].pop("enabled")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-enabled" --is-upgrade >"$WORK/missing-enabled.out" 2>&1; then
    echo "FAIL: missing kueue.enabled rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.armed ==="
cp -R "$CHART_DIR" "$WORK/chart-without-armed"
python3 - "$WORK/chart-without-armed/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values["kueue"].pop("armed")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-armed" --is-upgrade >"$WORK/missing-armed.out" 2>&1; then
    echo "FAIL: missing kueue.armed rendered successfully" >&2
    exit 1
fi

echo "=== invalid kueue.namespaceLabelledExternally ==="
if render "$WORK/ack-string.out" --set-string kueue.namespaceLabelledExternally=yes 2>&1; then
    echo "FAIL: a non-boolean kueue.namespaceLabelledExternally rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.namespaceLabelledExternally ==="
cp -R "$CHART_DIR" "$WORK/chart-without-ack"
python3 - "$WORK/chart-without-ack/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values["kueue"].pop("namespaceLabelledExternally")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-ack" --is-upgrade >"$WORK/missing-ack.out" 2>&1; then
    echo "FAIL: missing kueue.namespaceLabelledExternally rendered successfully" >&2
    exit 1
fi

echo "=== missing kueue.buildPods ==="
cp -R "$CHART_DIR" "$WORK/chart-without-kueue"
python3 - "$WORK/chart-without-kueue/values.yaml" <<'PY'
import sys
import yaml

path = sys.argv[1]
values = yaml.safe_load(open(path, encoding="utf-8"))
values.pop("kueue")
with open(path, "w", encoding="utf-8") as output:
    yaml.safe_dump(values, output, sort_keys=False)
PY
if helm template kueue-topology-test "$WORK/chart-without-kueue" --is-upgrade >"$WORK/missing.out" 2>&1; then
    echo "FAIL: missing kueue.buildPods rendered successfully" >&2
    exit 1
fi

echo "=== kueue.armed ships false ==="
python3 - "$CHART_DIR/values.yaml" <<'PY'
import sys
import yaml

values = yaml.safe_load(open(sys.argv[1], encoding="utf-8"))
assert values["kueue"]["armed"] is False, (
    f"kueue.armed must ship false, got {values['kueue']['armed']!r}"
)
assert values["kueue"]["enabled"] is False, (
    f"kueue.enabled must ship false, got {values['kueue']['enabled']!r}"
)
# The escape hatch is opt-in too. Shipping it true would pre-acknowledge a claim
# no operator ever made, which is exactly the silence this gate replaced.
assert values["kueue"]["namespaceLabelledExternally"] is False, (
    "kueue.namespaceLabelledExternally must ship false, got "
    f"{values['kueue']['namespaceLabelledExternally']!r}"
)
PY

# ---------------------------------------------------------------------------
# The zero-capture gate must actually discriminate.
# ---------------------------------------------------------------------------
# deploy/kueue/zero-capture-gate.sh is a live-cluster gate. Run the REAL script
# here with a fake kubectl whose namespace answer is derived from a REAL render
# of this chart, so "the gate passes" and "the gate fails" are decided by the
# chart, not by a hand-written fixture. Without this, arming could silently
# change the Namespace label while the gate kept reporting inert.
GATE="$REPO_ROOT/deploy/kueue/zero-capture-gate.sh"
if [ ! -f "$GATE" ]; then
    echo "FAIL: zero-capture gate is missing: $GATE" >&2
    exit 1
fi

GATE_BIN="$WORK/gate-bin"
mkdir -p "$GATE_BIN"
cat >"$GATE_BIN/kubectl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
args=" $* "
if [[ "$args" == *' get namespace djinn '* ]]; then
    cat "$FAKE_NAMESPACE_LABEL"
elif [[ "$args" == *' get pods '* ]]; then
    printf 'Running'
fi
EOF
cat >"$GATE_BIN/helm" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$GATE_BIN/kubectl" "$GATE_BIN/helm"

run_gate_against_render() {
    local name=$1 expectation=$2
    shift 2
    local dir="$WORK/gate-$name"
    mkdir -p "$dir"
    helm template djinn-inert-kueue-gate "$CHART_DIR" --is-upgrade "$@" >"$dir/render.yaml"
    python3 - "$dir/render.yaml" >"$dir/namespace-label.txt" <<'PY'
import sys
import yaml

docs = [doc for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if doc]
namespaces = [doc for doc in docs if doc.get("kind") == "Namespace"]
assert len(namespaces) == 1, f"expected one Namespace, got {len(namespaces)}"
label = namespaces[0].get("metadata", {}).get("labels", {}).get("djinn.io/kueue-managed", "")
sys.stdout.write(str(label))
PY
    set +e
    FAKE_NAMESPACE_LABEL="$dir/namespace-label.txt" \
    KUBECTL="$GATE_BIN/kubectl" \
    HELM="$GATE_BIN/helm" \
    KUEUE_GATE_POLL_SECONDS=0 \
        bash "$GATE" \
            --context fake-disposable \
            --designated-operator-secret fake-designated-operator \
            --timeout-seconds 1 >"$dir/out" 2>&1
    local status=$?
    set -e
    if [ "$expectation" = pass ] && [ "$status" -ne 0 ]; then
        echo "FAIL: zero-capture gate rejected the $name render" >&2
        cat "$dir/out" >&2
        exit 1
    fi
    if [ "$expectation" = fail ] && [ "$status" -eq 0 ]; then
        echo "FAIL: zero-capture gate accepted the $name render; it does not detect the armed state" >&2
        cat "$dir/out" >&2
        exit 1
    fi
}

echo "=== zero-capture gate passes against the default render ==="
run_gate_against_render default pass

echo "=== zero-capture gate FAILS against an armed render ==="
run_gate_against_render armed fail --set kueue.enabled=true --set kueue.armed=true
grep -q 'djinn.io/kueue-managed=true' "$WORK/gate-armed/out" || {
    echo "FAIL: the armed gate run failed for some other reason than capture:" >&2
    cat "$WORK/gate-armed/out" >&2
    exit 1
}

echo "=== All Kueue topology Helm render tests passed ==="
