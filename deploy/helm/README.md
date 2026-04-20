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
- For local dev: `tilt`, `kind`, and `docker`.

## Install order (production / manual)

```bash
helm install djinn-crds deploy/helm/djinn-crds
helm install djinn       deploy/helm/djinn \
  --namespace djinn --create-namespace
```

## Local kind workflow

Use Tilt — the `Tiltfile` at the repo root bootstraps the kind cluster +
localhost:5001 registry, builds both images, installs the Helm release with
`values.local.yaml`, and port-forwards the API/UI (`:3000`), worker RPC
(`:8443`), Dolt (`:3306`), and Qdrant (`:6333`/`:6334`) for you:

```bash
tilt up         # full stack up, watched, port-forwards live in the Tilt UI
tilt down       # uninstall the Helm release (kind cluster survives)
kind delete cluster --name djinn   # tear the cluster down entirely
```

`djinn-server` rebuilds + rolls automatically on changes under `server/`.
`djinn-agent-runtime` rebuilds when its Dockerfile or `server/` sources
change and is pushed under the stable `:dev` tag the chart ConfigMap
references.

Before `tilt up`, create the GitHub App Secret the chart expects (it's
referenced as `existingSecret` in `values.local.yaml`):

```bash
kubectl create namespace djinn
kubectl -n djinn create secret generic djinn-github-app \
  --from-literal=appId=... \
  --from-literal=privateKey="$(cat path/to/private-key.pem)" \
  --from-literal=clientId=... \
  --from-literal=clientSecret=...
```

`values.local.yaml` swaps RWX PVCs for hostPath mounts, pins the local
registry's image refs, and tightens resource requests so the whole stack
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
