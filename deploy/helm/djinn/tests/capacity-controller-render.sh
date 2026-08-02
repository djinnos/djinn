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
for background in by_kind["ClusterQueue"][1:]:
  assert background["metadata"]["labels"]["djinn.io/quota-owner"] == "warm-borrow"

seen=set()
for kind in ("Deployment","StatefulSet"):
  for obj in by_kind.get(kind,[]):
    labels=obj.get("spec",{}).get("template",{}).get("metadata",{}).get("labels",{})
    if labels.get("djinn.io/capacity-reserved") == "true": seen.add(f"{kind}/{obj['metadata']['name']}")
for suffix in ("-server", "-postgres", "-qdrant", "-zot", "-buildkitd"):
  assert any(name.endswith(suffix) for name in seen), f"missing protected workload {suffix}: {seen}"

role=next(r for r in by_kind["ClusterRole"] if r["metadata"]["name"].endswith("-capacity"))
rules={(tuple(x["apiGroups"]),tuple(x["resources"])):set(x["verbs"]) for x in role["rules"]}
assert rules[(('',),('nodes',))] == {'get','list','watch'}
assert rules[(('',),('pods',))] == {'get','list','watch'}
assert rules[(('kueue.x-k8s.io',),('clusterqueues',))] == {'get','list','watch','patch'}
assert all('*' not in groups+resources and '*' not in verbs for (groups,resources),verbs in rules.items())
PY

if helm template capacity-test "$CHART_DIR" --is-upgrade --set kueue.enabled=true --set kueue.capacity.enabled=true >"$WORK/rejected" 2>&1; then
  echo "FAIL: enabled controller rendered without a node selector" >&2; exit 1
fi
grep -q 'requires capacity.nodeSelector' "$WORK/rejected"

echo "PASS: derived capacity Helm ownership, protected inputs, queue safety, selector, and RBAC"
