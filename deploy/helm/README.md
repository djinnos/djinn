# Djinn Helm charts

Phase 2 installs Djinn on top of Kubernetes via three charts:

- `djinn-prereqs/` — cluster-scoped **third-party** prerequisites, pinned to
  upstream releases. Today: Kueue `0.19.0`. Required at stock values; install
  as its own release before `djinn`, exactly like cert-manager. See its
  [README](djinn-prereqs/README.md) and
  [deploy/kueue/README.md](../kueue/README.md).
- `djinn-crds/` — reserved for **Djinn's own** future CustomResourceDefinitions.
  Install first, upgrade independently. Empty in the current release. Not a
  home for third-party operators.
- `djinn/` — the workload: djinn-server controller Deployment, bundled
  Postgres 16 + Qdrant StatefulSets, per-task-run RBAC, PVCs, and secrets.

## Prerequisites

- `kubectl` >= 1.29
- `helm` >= 3.14
- The `djinn-prereqs` release, and a cluster at **Kubernetes >= 1.30** (a Kueue
  0.19 requirement that the upstream chart does not declare, so Helm will not
  check it; 1.29 rejects its CRDs, and the floor is 1.30 rather than 1.34 only
  because Djinn's values disable the DRA feature gates — see
  [deploy/kueue/README.md](../kueue/README.md#minimum-kubernetes-is-130-and-only-because-dra-is-disabled)).
  This is a hard prerequisite, not an option: the `djinn` chart defaults to
  `kueue.enabled: true` and `kueue.armed: true`, so it renders
  `kueue.x-k8s.io/v1beta1` objects and Kueue admits Jobs against the configured
  CPU/memory/Pods vector. At stock static values, `buildPods` is one finite
  fallback bound; with `kueue.capacity.contract: vector-v1`, the controller
  replaces the vector from eligible Nodes, and Pods becomes their real
  post-reserve Kubernetes Pod ceiling rather than a build-shaped concurrency
  limit. Without Kueue the `djinn` install is refused by
  `djinn/templates/prereq-guard.yaml`, which names this chart in the error.
- Nodes that have passed
  `deploy/node/k3s/djinn-cgroup-writable-conformance.sh`. Task-run writable
  cgroups are on by default and assign `RuntimeClass/djinn-cgroup-writable`,
  whose `scheduling.nodeSelector` requires `djinn.io/cgroup-writable=true`; on
  a cluster with no such node every task-run Pod stays Pending forever. The
  conformance script owns that marker and applies it itself once the node
  passes — never apply it by hand. The prereq guard refuses the install when no
  node carries it.
- A Kubernetes cluster. For production deploys, ensure a StorageClass that
  satisfies `ReadWriteMany` is available (the `mirrors` and `cache` PVCs
  default to RWX so the mirror cache can be shared across task-run Pods on
  multi-node clusters). For single-node dev clusters (kind, k3s on a
  laptop), `values.local.yaml` swaps PVCs for hostPath volumes.
- For local dev: `tilt`, `kind`, and `docker`.

## Install order (production / manual)

```bash
# Required: the djinn chart renders the Kueue topology at stock values.
# --wait matters — ClusterQueue/LocalQueue carry a conversion webhook, so the
# controller must be serving before djinn applies the topology.
helm install djinn-prereqs deploy/helm/djinn-prereqs \
  --namespace kueue-system --create-namespace --wait

helm install djinn-crds deploy/helm/djinn-crds
helm install djinn       deploy/helm/djinn \
  --namespace djinn --create-namespace
```

For a cluster that will never have these prerequisites (kind, a laptop, a
direct-worker profile), opt out of the armed profile explicitly. The switches
only make sense together — half of them is the incoherent pairing that
`djinn/templates/deployment-server.yaml` refuses at render time:

```bash
helm install djinn deploy/helm/djinn --namespace djinn --create-namespace \
  --set cgroupLauncher.mode=disabled \
  --set cgroupWritable.runtimeClass.enabled=false \
  --set cgroupWritable.taskRuns.enabled=false \
  --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1 \
  --set kueue.enabled=false \
  --set kueue.armed=false
```

`djinn/values.local.yaml` is exactly that profile, and Tilt uses it.

## Local kind workflow

Use Tilt — the `Tiltfile` at the repo root bootstraps the kind cluster +
localhost:5001 registry, builds both images, installs the Helm release with
`values.local.yaml`, and port-forwards the API/UI (`:3000`), worker RPC
(`:8443`), Postgres (`:5432`), and Qdrant (`:6333`/`:6334`) for you:

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
- Bundled Postgres and Qdrant StatefulSets use RWO for their own per-pod
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
