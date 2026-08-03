#!/usr/bin/env bash
# Prove the one-release chart/controller capacity compatibility matrix without
# downloading an old release. The old-chart wire is checked in beside this test.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURE="$SCRIPT_DIR/fixtures/capacity-old-chart-pr2901.yaml"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A complete dedicated pool is the strongest identity case: the ResourceFlavor,
# controller selector, task node selector, and task toleration must all name it.
helm template capacity-vector "$CHART_DIR" --is-upgrade \
  --set kueue.enabled=true \
  --set kueue.capacity.enabled=true \
  --set kueue.capacity.contract=vector-v1 \
  --set kueue.capacity.source=nodepool-limits \
  --set kueue.capacity.nodePool.dedicated=true \
  --set kueue.capacity.nodePool.name=vector-pool >"$WORK/vector.yaml"

helm template capacity-static "$CHART_DIR" --is-upgrade \
  --set kueue.enabled=true \
  --set kueue.capacity.enabled=true \
  --set kueue.capacity.contract=vector-v1 \
  --set kueue.capacity.source=static \
  --set-string kueue.capacity.flavorSelector.key=workload-type \
  --set-string kueue.capacity.flavorSelector.value=djinn >"$WORK/static.yaml"

python3 - "$FIXTURE" "$WORK/vector.yaml" "$WORK/static.yaml" <<'PY'
import json
import pathlib
import sys
import yaml

old, vector, static = sys.argv[1:]
fixture = yaml.safe_load(open(old, encoding="utf-8"))
old_env = fixture["oldControllerEnvironment"]
assert "DJINN_CAPACITY_CONTRACT" not in old_env
assert old_env["DJINN_CAPACITY_IDLE_CPU"] == "750m"
assert fixture["clusterQueue"]["metadata"]["annotations"]["djinn.io/binding-resource"] == "pods"
quotas = {r["name"]: str(r["nominalQuota"])
          for r in fixture["clusterQueue"]["spec"]["resourceGroups"][0]["flavors"][0]["resources"]}
assert (quotas["pods"], quotas["cpu"], quotas["memory"]) == ("3", "10k", "100Ti")
# The new controller's parser has an explicit absent-marker Legacy branch; this
# fixture therefore cannot reinterpret the old sentinel cpu/memory quotas as a
# static vector.
controller = pathlib.Path("server/crates/djinn-k8s/src/capacity_controller.rs").read_text()
assert "Err(std::env::VarError::NotPresent) => CapacityContract::Legacy" in controller
assert "Never derive a vector from legacy sentinel-shaped quotas" in controller

def docs(path): return [d for d in yaml.safe_load_all(open(path, encoding="utf-8")) if d]
def one(items, kind, suffix=None):
    matches = [d for d in items if d["kind"] == kind and (suffix is None or d["metadata"]["name"].endswith(suffix))]
    assert len(matches) == 1, (kind, suffix, [d["metadata"]["name"] for d in matches])
    return matches[0]
def env(items):
    deployment = one(items, "Deployment", "-server")
    return {e["name"]: e["value"] for e in deployment["spec"]["template"]["spec"]["containers"][0]["env"] if "value" in e}
def capacity_queue(items):
    return next(q for q in items if q["kind"] == "ClusterQueue" and q["metadata"]["name"].endswith("-kueue"))
def karpenter_rules(items):
    role = one(items, "ClusterRole", "-capacity")
    return [r for r in role["rules"] if r["apiGroups"] == ["karpenter.sh"]]

vdocs, sdocs = docs(vector), docs(static)
venv, senv = env(vdocs), env(sdocs)
# Old controller + new chart: finite vector plus retained PR #2901 seam.
for name in ("DJINN_CAPACITY_IDLE_CPU", "DJINN_CAPACITY_COMPILE_CPU", "DJINN_CAPACITY_FAIL_SAFE_PODS", "DJINN_CAPACITY_FAIL_SAFE_COMPILE_SLOTS"):
    assert name in venv, name
vqueue = capacity_queue(vdocs)
assert vqueue["metadata"]["annotations"]["djinn.io/binding-resource"] == "pods"
vquotas = {r["name"]: str(r["nominalQuota"])
           for r in vqueue["spec"]["resourceGroups"][0]["flavors"][0]["resources"]}
assert (vquotas["cpu"], vquotas["memory"], vquotas["pods"]) == ("12", "48Gi", "3")
# New + new: only the complete declaration emits the marker and all source,
# static, and ownership fields; all pool identities remain identical.
assert venv["DJINN_CAPACITY_CONTRACT"] == "vector-v1"
assert (venv["DJINN_CAPACITY_SOURCE"], venv["DJINN_CAPACITY_STATIC_CPU"], venv["DJINN_CAPACITY_STATIC_MEMORY"], venv["DJINN_CAPACITY_STATIC_PODS"]) == ("nodepool-limits", "12", "48Gi", "3")
pool = "vector-pool"
assert json.loads(venv["DJINN_CAPACITY_FLAVOR_SELECTOR"]) == {"karpenter.sh/nodepool": pool}
assert one(vdocs, "ResourceFlavor")["spec"] == {"nodeLabels": {"karpenter.sh/nodepool": pool}}
assert json.loads(venv["DJINN_K8S_NODE_SELECTOR"]) == {"karpenter.sh/nodepool": pool}
assert {"key":"djinn.io/task-pool", "operator":"Equal", "value":pool, "effect":"NoSchedule"} in json.loads(venv["DJINN_K8S_TOLERATIONS"])
assert karpenter_rules(vdocs) == [{"apiGroups":["karpenter.sh"], "resources":["nodepools"], "verbs":["get","list","watch"]}]
# Static vector mode owns its explicit flavor but never gains a Karpenter watch.
assert senv["DJINN_CAPACITY_CONTRACT"] == "vector-v1"
assert json.loads(senv["DJINN_CAPACITY_FLAVOR_SELECTOR"]) == {"workload-type":"djinn"}
assert one(sdocs, "ResourceFlavor")["spec"] == {"nodeLabels":{"workload-type":"djinn"}}
assert not karpenter_rules(sdocs)
print("PASS: old/new controller-chart matrix, finite vector, and identity contract")
PY

reject() {
  local name=$1
  shift
  if helm template "capacity-invalid-$name" "$CHART_DIR" --is-upgrade "$@" >"$WORK/$name.out" 2>&1; then
    echo "FAIL: accepted invalid vector-v1 activation: $name" >&2
    exit 1
  fi
}

# Complete declaration is fail-closed: malformed/missing dimensions, ownership,
# dedication, and task scheduling identity are all rejected by their responsible
# schema or template seam.
for dimension in cpu memory pods; do
  reject "invalid-static-$dimension" --set kueue.enabled=true --set kueue.capacity.enabled=true --set kueue.capacity.contract=vector-v1 --set kueue.capacity.source=static --set-string kueue.capacity.flavorSelector.key=workload-type --set-string kueue.capacity.flavorSelector.value=djinn --set-string "kueue.capacity.staticFallback.$dimension="
done
reject missing-static-ownership --set kueue.enabled=true --set kueue.capacity.enabled=true --set kueue.capacity.contract=vector-v1 --set kueue.capacity.source=static
reject missing-nodepool-dedication --set kueue.enabled=true --set kueue.capacity.enabled=true --set kueue.capacity.contract=vector-v1 --set kueue.capacity.source=nodepool-limits --set kueue.capacity.nodePool.dedicated=false --set kueue.capacity.nodePool.name=vector-pool
reject mismatched-task-pool --set kueue.enabled=true --set kueue.capacity.enabled=true --set kueue.capacity.contract=vector-v1 --set kueue.capacity.source=nodepool-limits --set kueue.capacity.nodePool.dedicated=true --set kueue.capacity.nodePool.name=vector-pool --set-string 'resources.taskrun.nodeSelector.karpenter\.sh/nodepool=other-pool'

# A release marker is never inferred by static/source values alone.
helm template capacity-unmarked "$CHART_DIR" --is-upgrade \
  --set kueue.enabled=true --set kueue.capacity.enabled=true --set kueue.capacity.source=static >"$WORK/unmarked.yaml"
if grep -q 'DJINN_CAPACITY_CONTRACT' "$WORK/unmarked.yaml"; then
  echo "FAIL: unmarked capacity configuration emitted vector-v1" >&2
  exit 1
fi

echo "PASS: vector-v1 only activates with a complete declaration"
