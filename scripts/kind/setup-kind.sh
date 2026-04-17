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

Next steps:
  make image               # build djinn images locally
  make image-push-local    # retag + push to localhost:${REG_PORT}
  make helm-install-local  # install djinn-crds + djinn into the cluster

Tear down with: make kind-down
EOF
