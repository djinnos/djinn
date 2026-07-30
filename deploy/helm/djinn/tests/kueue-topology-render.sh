#!/usr/bin/env bash
# Structural Helm render contract for the inert Kueue prerequisite topology.
# Usage: bash deploy/helm/djinn/tests/kueue-topology-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

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

assert_topology() {
    local manifest=$1 expected_pods=$2
    python3 - "$manifest" "$expected_pods" <<'PY'
import sys
import yaml

manifest, expected_pods = sys.argv[1:]
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
assert spec.get("queueingStrategy") == "StrictFIFO", "ClusterQueue must use StrictFIFO"
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

local_queues = named("LocalQueue")
assert len(local_queues) == 2, f"expected task-run and warm LocalQueues, got {len(local_queues)}"
local_names = {local["metadata"]["name"] for local in local_queues}
assert local_names == {"kueue-topology-test-djinn-task-run", "kueue-topology-test-djinn-warm"}
assert all(local.get("spec", {}).get("clusterQueue") == queue["metadata"]["name"] for local in local_queues)

namespaces = named("Namespace")
assert len(namespaces) == 1, f"expected one Namespace, got {len(namespaces)}"
assert "djinn.io/kueue-managed" not in namespaces[0].get("metadata", {}).get("labels", {}), "Namespace must remain unlabelled for Kueue"

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

deployments = named("Deployment")
assert len(deployments) == 1, "expected one controller Deployment"
containers = deployments[0]["spec"]["template"]["spec"]["containers"]
server = next(container for container in containers if container["name"] == "djinn-server")
values = {entry["name"]: entry.get("value") for entry in server["env"]}
assert values["DJINN_BUILD_ADMISSION_MODE"] == "observe", "buildAdmission mode changed"
assert values["DJINN_MAX_BUILD_TASKRUNS"] == "3", "buildAdmission cap changed"
PY
}

echo "=== valid Kueue topology ==="
render "$WORK/valid.yaml" --set kueue.buildPods=7
assert_topology "$WORK/valid.yaml" 7

expect_rejected() {
    local name=$1
    shift
    echo "=== invalid kueue.buildPods: $name ==="
    if render "$WORK/$name.out" "$@" 2>&1; then
        echo "FAIL: invalid Kueue scenario '$name' rendered successfully" >&2
        exit 1
    fi
}

expect_rejected zero --set kueue.buildPods=0
expect_rejected fractional --set kueue.buildPods=1.5
expect_rejected string --set-string kueue.buildPods=seven

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

echo "=== All Kueue topology Helm render tests passed ==="
