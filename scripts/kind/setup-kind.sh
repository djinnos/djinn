#!/usr/bin/env bash
# Bring up a kind cluster named `${CLUSTER_NAME:-djinn}` backed by a local
# containerd registry at 127.0.0.1:5001. Mirrors the inner-loop pattern used
# by upstream kagent — see scripts/kind/setup-kind.sh in kagent for the
# reference implementation.
#
# Prerequisites: docker, kind, kubectl.
#
# Idempotent: if the cluster or registry already exist, they're left alone.

set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-djinn}"
KIND_IMAGE_VERSION="${KIND_IMAGE_VERSION:-1.31.0}"
REG_NAME="${REG_NAME:-kind-registry}"
REG_PORT="${REG_PORT:-5001}"

# --- 1. Ensure local registry is running ------------------------------------
if [ "$(docker inspect -f '{{.State.Running}}' "${REG_NAME}" 2>/dev/null || echo false)" != 'true' ]; then
  echo ">>> starting local registry ${REG_NAME} at 127.0.0.1:${REG_PORT}"
  docker run -d --restart=always \
    -p "127.0.0.1:${REG_PORT}:5000" \
    --network bridge \
    --name "${REG_NAME}" \
    registry:2
else
  echo ">>> registry ${REG_NAME} already running"
fi

# --- 2. Create kind cluster -------------------------------------------------
if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
  echo ">>> kind cluster '${CLUSTER_NAME}' already exists; skipping create"
else
  echo ">>> creating kind cluster '${CLUSTER_NAME}'"
  cat <<EOF | kind create cluster --name "${CLUSTER_NAME}" --image "kindest/node:v${KIND_IMAGE_VERSION}" --config=-
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".registry]
      config_path = "/etc/containerd/certs.d"
nodes:
  - role: control-plane
EOF
fi

# --- 3. Wire up containerd registry config on each node ---------------------
REGISTRY_DIR="/etc/containerd/certs.d/localhost:${REG_PORT}"
for node in $(kind get nodes --name "${CLUSTER_NAME}"); do
  docker exec "${node}" mkdir -p "${REGISTRY_DIR}"
  cat <<EOF | docker exec -i "${node}" cp /dev/stdin "${REGISTRY_DIR}/hosts.toml"
[host."http://${REG_NAME}:5000"]
EOF
done

# --- 3b. Teach the kind node's containerd about the in-cluster Zot host ---
# kubelet runs on the node's host network and has no cluster-DNS resolver, so
# it can't look up `<release>-zot.<ns>.svc.cluster.local` natively. Mirror the
# Service hostname to its ClusterIP over plain HTTP (Zot serves HTTP; the
# `http = true` buildkit toggle mirrors this on the push side). Re-runs
# tolerate an existing file because the Service IP is stable per release.
#
# Only applies when the chart is installed with default DNS shape
# (<release>-zot.<ns>.svc.cluster.local:5000). For prod/EKS where Zot is
# fronted by a real DNS ingress, this block is a no-op: `kubectl get svc`
# returns nothing and we silently skip.
ZOT_NS="${ZOT_NAMESPACE:-djinn}"
ZOT_SVC="${ZOT_SERVICE:-djinn-zot}"
ZOT_HOST="${ZOT_HOST:-${ZOT_SVC}.${ZOT_NS}.svc.cluster.local:5000}"
ZOT_IP="$(kubectl -n "${ZOT_NS}" get svc "${ZOT_SVC}" -o jsonpath='{.spec.clusterIP}' 2>/dev/null || true)"
if [ -n "${ZOT_IP}" ]; then
  echo ">>> wiring kubelet → Zot mirror: ${ZOT_HOST} → http://${ZOT_IP}:5000"
  ZOT_DIR="/etc/containerd/certs.d/${ZOT_HOST}"
  for node in $(kind get nodes --name "${CLUSTER_NAME}"); do
    docker exec "${node}" mkdir -p "${ZOT_DIR}"
    cat <<EOF | docker exec -i "${node}" cp /dev/stdin "${ZOT_DIR}/hosts.toml"
[host."http://${ZOT_IP}:5000"]
  capabilities = ["pull", "resolve"]
  skip_verify = true
EOF
  done
else
  echo ">>> Zot Service not found yet at ${ZOT_NS}/${ZOT_SVC} — skipping kubelet mirror (chart not installed?)"
fi

# --- 4. Attach the registry to the kind network ----------------------------
if [ "$(docker inspect -f '{{json .NetworkSettings.Networks.kind}}' "${REG_NAME}" 2>/dev/null)" = 'null' ]; then
  docker network connect "kind" "${REG_NAME}"
fi

# --- 5. Document the local registry in-cluster -----------------------------
# See https://github.com/kubernetes/enhancements/tree/master/keps/sig-cluster-lifecycle/generic/1755-communicating-a-local-registry
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: ConfigMap
metadata:
  name: local-registry-hosting
  namespace: kube-public
data:
  localRegistryHosting.v1: |
    host: "localhost:${REG_PORT}"
    help: "https://kind.sigs.k8s.io/docs/user/local-registry/"
EOF

cat <<EOF

>>> kind cluster '${CLUSTER_NAME}' ready.

Normally invoked by the Tiltfile (bootstrap resource 'kind-cluster'); run
directly only when you want the cluster without starting Tilt.

Next step: tilt up

Tear down with: kind delete cluster --name ${CLUSTER_NAME}
EOF
