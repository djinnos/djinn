#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

helm template capacity-test "$CHART_DIR" --is-upgrade \
  --set kueue.enabled=true \
  --set kueue.capacity.enabled=true \
  --set kueue.capacity.nodeSelector.key=kubernetes.io/hostname \
  --set kueue.capacity.nodeSelector.value=worker-1 \
  --set imagePipeline.zot.enabled=true >"$WORK/render.yaml"

# Do not advertise a Karpenter API version: dedicated rendering is selected by
# values alone, never by CRD discovery.
helm template capacity-nodepool "$CHART_DIR" --is-upgrade \
  --set kueue.enabled=true \
  --set kueue.capacity.enabled=true \
  --set kueue.capacity.source=nodepool-limits \
  --set kueue.capacity.nodePool.dedicated=true \
  --set kueue.capacity.nodePool.name=task-pool \
  --set-string kueue.capacity.staticFallback.cpu=12 \
  --set-string kueue.capacity.staticFallback.memory=48Gi \
  --set kueue.capacity.staticFallback.pods=3 >"$WORK/nodepool.yaml"

helm template capacity-nodepool "$CHART_DIR" --is-upgrade \
  --api-versions karpenter.sh/v1 \
  --set kueue.enabled=true \
  --set kueue.capacity.enabled=true \
  --set kueue.capacity.source=nodepool-limits \
  --set kueue.capacity.nodePool.dedicated=true \
  --set kueue.capacity.nodePool.name=task-pool \
  --set-string kueue.capacity.staticFallback.cpu=12 \
  --set-string kueue.capacity.staticFallback.memory=48Gi \
  --set kueue.capacity.staticFallback.pods=3 >"$WORK/nodepool-with-api.yaml"
cmp "$WORK/nodepool.yaml" "$WORK/nodepool-with-api.yaml"

helm template capacity-static "$CHART_DIR" --is-upgrade \
  --set kueue.enabled=true \
  --set kueue.capacity.enabled=true \
  --set kueue.capacity.source=static >"$WORK/static.yaml"

python3 - "$WORK/render.yaml" <<'PY'
import sys, yaml
docs=[d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
by_kind={}
for d in docs: by_kind.setdefault(d.get("kind"), []).append(d)

queue=by_kind["ClusterQueue"][0]
assert queue["metadata"]["labels"]["djinn.io/quota-owner"] == "derived-capacity"
assert queue["metadata"]["annotations"]["djinn.io/binding-resource"] == "pods"
assert "stopPolicy" not in queue["spec"]
assert "withinClusterQueue" not in queue["spec"].get("preemption", {})
assert by_kind["ResourceFlavor"][0]["spec"] == {}
for background in by_kind["ClusterQueue"][1:]:
  assert background["metadata"]["labels"]["djinn.io/quota-owner"] == "warm-borrow"

seen=set()
protected_cpu=0
def cpu_m(v):
  s=str(v)
  return int(s[:-1]) if s.endswith('m') else int(s)*1000
for kind in ("Deployment","StatefulSet"):
  for obj in by_kind.get(kind,[]):
    labels=obj.get("spec",{}).get("template",{}).get("metadata",{}).get("labels",{})
    if labels.get("djinn.io/capacity-reserved") == "true":
      seen.add(f"{kind}/{obj['metadata']['name']}")
      spec=obj["spec"]["template"]["spec"]
      regular=sum(cpu_m(c.get("resources",{}).get("requests",{}).get("cpu",0)) for c in spec.get("containers",[]))
      init=max([cpu_m(c.get("resources",{}).get("requests",{}).get("cpu",0)) for c in spec.get("initContainers",[])]+[0])
      protected_cpu += max(regular, init)
for suffix in ("-server", "-postgres", "-qdrant", "-zot", "-buildkitd"):
  assert any(name.endswith(suffix) for name in seen), f"missing protected workload {suffix}: {seen}"
assert protected_cpu > 0, "protected workload scheduler-effective CPU sum must be non-zero"
print(f"protected scheduler-effective CPU sum: {protected_cpu}m")

role=next(r for r in by_kind["ClusterRole"] if r["metadata"]["name"].endswith("-capacity"))
rules={(tuple(x["apiGroups"]),tuple(x["resources"])):set(x["verbs"]) for x in role["rules"]}
assert rules[(('',),('nodes',))] == {'get','list','watch'}
assert rules[(('',),('pods',))] == {'get','list','watch'}
assert rules[(('kueue.x-k8s.io',),('clusterqueues',))] == {'get','list','watch','patch'}
assert (('karpenter.sh',),('nodepools',)) not in rules
assert all('*' not in groups+resources and '*' not in verbs for (groups,resources),verbs in rules.items())
PY

python3 - "$WORK/nodepool.yaml" "$WORK/static.yaml" <<'PY'
import json, sys, yaml

def docs(path): return [d for d in yaml.safe_load_all(open(path)) if d]
def one(items, kind, suffix=None):
    matches = [d for d in items if d['kind'] == kind and (not suffix or d['metadata']['name'].endswith(suffix))]
    assert len(matches) == 1, (kind, suffix, [d['metadata']['name'] for d in matches])
    return matches[0]
def env(deployment):
    return {e['name']: e['value'] for e in deployment['spec']['template']['spec']['containers'][0]['env'] if 'value' in e}

nodepool, static = docs(sys.argv[1]), docs(sys.argv[2])
pool = 'task-pool'
assert one(nodepool, 'ResourceFlavor')['spec'] == {'nodeLabels': {'karpenter.sh/nodepool': pool}}
role = one(nodepool, 'ClusterRole', '-capacity')
assert [r for r in role['rules'] if r['apiGroups'] == ['karpenter.sh']] == [
    {'apiGroups': ['karpenter.sh'], 'resources': ['nodepools'], 'verbs': ['get', 'list', 'watch']}]
settings = env(one(nodepool, 'Deployment', '-server'))
assert settings['DJINN_CAPACITY_SOURCE'] == 'nodepool-limits'
assert (settings['DJINN_CAPACITY_STATIC_CPU'], settings['DJINN_CAPACITY_STATIC_MEMORY'], settings['DJINN_CAPACITY_STATIC_PODS']) == ('12', '48Gi', '3')
assert settings['DJINN_CAPACITY_NODEPOOL_NAME'] == pool
assert settings['DJINN_CAPACITY_NODEPOOL_DEDICATED'] == 'true'
assert json.loads(settings['DJINN_CAPACITY_FLAVOR_SELECTOR']) == {'karpenter.sh/nodepool': pool}
assert json.loads(settings['DJINN_K8S_NODE_SELECTOR']) == {'karpenter.sh/nodepool': pool}
assert {'key': 'djinn.io/task-pool', 'operator': 'Equal', 'value': pool, 'effect': 'NoSchedule'} in json.loads(settings['DJINN_K8S_TOLERATIONS'])

assert one(static, 'ResourceFlavor')['spec'] == {}
static_role = one(static, 'ClusterRole', '-capacity')
assert not any(r['apiGroups'] == ['karpenter.sh'] for r in static_role['rules'])
static_env = env(one(static, 'Deployment', '-server'))
assert static_env['DJINN_CAPACITY_SOURCE'] == 'static'
assert not any(n.startswith('DJINN_CAPACITY_NODEPOOL') or n == 'DJINN_CAPACITY_FLAVOR_SELECTOR' for n in static_env)
print('dedicated NodePool identity, isolation, and conditional watcher RBAC verified')
PY

# The launcher Job is produced by djinn-k8s rather than by Helm. Keep its
# explicit no-restart resize policy in the same PR-lane chart contract that
# protects the controller's prerequisite configuration.
grep -q 'resource_name: "cpu"' "$CHART_DIR/../../../server/crates/djinn-k8s/src/launcher.rs"
grep -q 'restart_policy: "NotRequired"' "$CHART_DIR/../../../server/crates/djinn-k8s/src/launcher.rs"

if helm template capacity-test "$CHART_DIR" --is-upgrade --set kueue.enabled=true --set kueue.capacity.enabled=true >"$WORK/rejected" 2>&1; then
  echo "FAIL: enabled controller rendered without a node selector" >&2; exit 1
fi
grep -q 'requires capacity.nodeSelector' "$WORK/rejected"

for invalid in 'kueue.capacity.nodePool.dedicated=false' 'kueue.capacity.nodePool.name='; do
  if helm template capacity-nodepool-invalid "$CHART_DIR" --is-upgrade \
    --set kueue.enabled=true \
    --set kueue.capacity.source=nodepool-limits \
    --set "$invalid" >"$WORK/nodepool-invalid" 2>&1; then
    echo "FAIL: nodepool-limits rendered with $invalid" >&2; exit 1
  fi
done

if helm template capacity-nodepool-mismatch "$CHART_DIR" --is-upgrade \
  --set kueue.enabled=true \
  --set kueue.capacity.source=nodepool-limits \
  --set kueue.capacity.nodePool.dedicated=true \
  --set kueue.capacity.nodePool.name=task-pool \
  --set-string 'resources.taskrun.nodeSelector.karpenter\.sh/nodepool=other-pool' >"$WORK/nodepool-mismatch" 2>&1; then
  echo "FAIL: nodepool-limits rendered with mismatched task nodepool identity" >&2; exit 1
fi
grep -q 'must match resources.taskrun.nodeSelector.karpenter.sh/nodepool' "$WORK/nodepool-mismatch"

if grep -q '\.Capabilities\.APIVersions.*karpenter\|karpenter.*\.Capabilities\.APIVersions' \
  "$CHART_DIR/templates/kueue-topology.yaml" \
  "$CHART_DIR/templates/clusterrole-capacity.yaml" \
  "$CHART_DIR/templates/deployment-server.yaml"; then
  echo "FAIL: Karpenter CRD presence controls capacity source rendering" >&2; exit 1
fi

echo "PASS: derived capacity Helm ownership, protected inputs, queue safety, NodePool identity, selector, and conditional RBAC"
