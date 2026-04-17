# Djinn Helm charts

Phase 2 installs Djinn on top of Kubernetes via two charts:

- `djinn-crds/` — reserved for future CustomResourceDefinitions. Install
  first, upgrade independently. Empty in the current release.
- `djinn/` — the workload: djinn-server controller Deployment, bundled
  Dolt + Qdrant StatefulSets, per-task-run RBAC, PVCs, and secrets.

## Prerequisites

- `kubectl` >= 1.29
- `helm` >= 3.14
- A Kubernetes cluster. For production deploys, ensure a StorageClass that
  satisfies `ReadWriteMany` is available (the `mirrors` and `cache` PVCs
  default to RWX so the mirror cache can be shared across task-run Pods on
  multi-node clusters). For single-node dev clusters (kind, k3s on a
  laptop), `values.local.yaml` swaps PVCs for hostPath volumes.
- For local dev: `kind` and `docker` (see `scripts/kind/setup-kind.sh`).

## Install order

```bash
helm install djinn-crds deploy/helm/djinn-crds
helm install djinn       deploy/helm/djinn \
  --namespace djinn --create-namespace
```

## Local kind workflow

```bash
make kind-up            # create kind cluster + local registry
make image              # build djinn-server + djinn-agent-runtime images
make image-push-local   # retag + push to localhost:5001
make helm-install-local # install both charts with values.local.yaml
make helm-uninstall     # tear down the release (cluster survives)
make kind-down          # delete the cluster
```

`values.local.yaml` swaps RWX PVCs for hostPath mounts, sets
`imagePullPolicy: Never`, and tightens resource requests so the whole stack
fits on a laptop.

## VPS vs multi-node cluster differences

Single-node (kind, k3s on a VPS):

- `storage.mirrors.hostPath` / `storage.cache.hostPath` is safe — only one
  node ever mounts the path.
- `storage.*.accessMode: ReadWriteOnce` works.

Multi-node:

- Leave `.hostPath` empty so PVCs render.
- Provide a `storageClassName` whose provisioner supports RWX (e.g. NFS,
  cephfs, longhorn configured for RWX, AWS EFS CSI).
- Bundled Dolt and Qdrant StatefulSets use RWO for their own per-pod
  volumes — independent of the mirror PVC story.

## Secrets

GitHub App credentials and the vault AES key are chart-managed by default.
For production deploys, point the chart at externally-managed Secrets:

```yaml
secrets:
  githubApp:
    existingSecret: my-github-app
  vaultKey:
    existingSecret: my-vault-key
```

The Deployment mounts both at `/var/run/secrets/djinn/` and the relevant
`*_PATH` env vars are set unconditionally.
