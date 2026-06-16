# Deploying djinn

djinn is a Kubernetes-native control plane. The server doesn't just *run* on
Kubernetes — it *uses* the Kubernetes API at runtime to dispatch every agent
task-run, graph-warm, and verification as a short-lived Job/Pod. So a real
cluster is a hard requirement, but it can be **any conformant Kubernetes**: a
single-node [k3s](https://k3s.io) box, local [kind](https://kind.sigs.k8s.io),
or a managed cluster (EKS / GKE / AKS) or self-managed kubeadm.

There is **one Helm chart** (`deploy/helm/djinn`) for every environment. The
only thing that changes between a laptop and production is whether the *backing
services* (Postgres, Qdrant, the image registry) are bundled in-cluster or
pointed at managed equivalents. Everything below is just different `values`.

- [How it installs (and migrates)](#how-it-installs-and-migrates)
- [Image tags](#image-tags)
- [Node prerequisites](#node-prerequisites)
- [Profiles](#profiles)
  - [Local (Tilt + kind)](#local-tilt--kind)
  - [Single node / self-hosted / VPS](#single-node--self-hosted--vps)
  - [Any managed or self-managed Kubernetes (production)](#any-managed-or-self-managed-kubernetes-production)
- [Values reference](#values-reference)
- [Upgrades](#upgrades)

---

## How it installs (and migrates)

```bash
helm upgrade --install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace \
  -f my-values.yaml
```

That single command works for a **fresh install or an upgrade**, with **bundled
or external Postgres**, on **any** cluster. The chart installs `djinn-server`
(the controller), optionally Postgres 16 and Qdrant, the BuildKit image
pipeline, RBAC for dispatching task-run Jobs, and the PVCs/secrets the
controller needs.

**Migrations are automatic.** The server Deployment runs `djinn-server
--migrate-only` in an **initContainer** before the app container starts. It
waits for Postgres to be reachable (~60s TCP retry, built into the binary),
takes the sqlx migrator advisory lock, applies pending migrations, then exits;
the app container boots and only *verifies* the schema (lock-free). With
`replicas: 1` and `maxSurge: 1` exactly one migrator runs at a time, and the
advisory lock serialises any overlap — so this is safe on rolling upgrades and
needs no manual step.

> Earlier versions ran migrations as a `pre-install`/`pre-upgrade` Helm **hook
> Job**. That only worked when Postgres was an already-running *external*
> database (the ArgoCD/production path), because Helm runs hooks *before* it
> creates the chart's own resources — so a fresh `helm install` with bundled
> Postgres deadlocked. The initContainer approach removed that footgun.

To run migrations by hand (debugging): `djinn-server --migrate-only` against
the DB, e.g. `kubectl run --rm -it djinn-migrate --image=<server-image>
--command -- djinn-server --migrate-only`.

## Image tags

Published images live at `ghcr.io/djinnos/*` and are **tagged without a leading
`v`** — e.g. `ghcr.io/djinnos/djinn-server:0.5.5`, not `:v0.5.5` (the `v*` form
is only the git/chart tag). Four images make up a release: `djinn-server`,
`djinn-agent-runtime`, `djinn-buildkitd`, `djinn-image-builder`. Pin
`image.server` + `image.runtime` for production; the buildkitd/builder refs
default to `:latest` and can be pinned via `imagePipeline.*`.

## Dispatch-state debug endpoint

Operators investigating a wedged dispatcher can inspect the admin-gated JSON
snapshot without scraping logs:

```bash
# Admin session cookie required; returns JSON with cooldowns, slot pool,
# breaker table, inflight ledger, pause state, and totals.
curl --cookie 'djinn_session=<admin-cookie>' \
  http://localhost:<port>/debug/dispatch-state

# If a runbook stores only the raw session token, pass it under the server's
# cookie name: `djinn_session=<admin-cookie>`.

# Missing/invalid session is rejected by the same admin gate as other admin APIs.
curl -i http://localhost:<port>/debug/dispatch-state
# HTTP/1.1 401 Unauthorized
```

## Node prerequisites

The image pipeline runs BuildKit **rootless** via user namespaces. Every node
that may schedule the `buildkitd` pod needs:

```sh
sysctl -w kernel.unprivileged_userns_clone=1
sysctl -w user.max_user_namespaces=28633   # or higher
```

Persist via `/etc/sysctl.d/`. k3s nodes usually ship with both; kind inherits
host sysctls; some managed node images need them set via a startup script or
DaemonSet. See [`deploy/helm/djinn/README.md`](../deploy/helm/djinn/README.md).

---

## Profiles

### Local (Tilt + kind)

For development, the whole stack (cluster, registry, server, Postgres, Qdrant,
image pipeline, Langfuse) comes up with `tilt up`. See
[Quick start (local)](../README.md#quick-start-local). This builds the binaries
and UI from your working tree and hot-reloads them.

### Single node / self-hosted / VPS

One box, everything bundled — Postgres, Qdrant, and the in-cluster
[Zot](https://zotregistry.dev) registry all run inside the cluster. Ideal for
self-hosting on a VPS with [k3s](https://k3s.io).

```bash
helm upgrade --install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace \
  --set postgres.enabled=true \
  --set qdrant.enabled=true \
  --set imagePipeline.zot.enabled=true \
  --set imagePipeline.buildkitd.zotPlaintext=true \
  --set ingress.enabled=true \
  --set ingress.className=traefik \
  --set ingress.host=djinn.example.com \
  --set env.publicUrl=https://djinn.example.com \
  -f my-values.yaml          # passwords, vault key, image tags, TLS
```

Notes for single-node:

- **Storage:** use `ReadWriteOnce` PVCs (RWO is enforced per-node, not per-pod,
  so it's fine when everything lands on one node). k3s ships the `local-path`
  provisioner.
- **TLS:** pair `ingress` with [cert-manager](https://cert-manager.io) and a
  `ClusterIssuer` annotation (`cert-manager.io/cluster-issuer`) for automatic
  Let's Encrypt certs through k3s's built-in Traefik.
- **kubelet → Zot:** the node's container runtime must be able to pull
  per-project images from the in-cluster Zot. Because Zot is a ClusterIP
  service the host-network kubelet can't resolve by DNS, mirror the Zot Service
  hostname to its ClusterIP in the runtime's registry config (k3s:
  `/etc/rancher/k3s/registries.yaml`).

> A companion Ansible project automates the whole single-VPS path — OS
> hardening (UFW, fail2ban, SSH), k3s, cert-manager, the Zot registry wiring,
> and this chart — for an Ubuntu box. Use it as a turnkey starting point or as a
> worked reference for the steps above.

### Any managed or self-managed Kubernetes (production)

Point the backing services at managed equivalents and the image pipeline at a
managed registry. This is how the EKS/ArgoCD deployment runs, but nothing here
is AWS-specific — the same shape applies to GKE, AKS, or self-managed clusters.

```yaml
# my-values.yaml
image:
  server: ghcr.io/djinnos/djinn-server:0.5.5
  runtime: ghcr.io/djinnos/djinn-agent-runtime:0.5.5

# External Postgres (RDS / Cloud SQL / managed). Provide ONE of:
postgres:
  enabled: false
database:
  existingSecret: djinn-db        # Secret with a `url` key; never lands in a ConfigMap
  # or: externalUrl: postgres://user:pass@host:5432/djinn?sslmode=require

# Managed registry (ECR / GCR / ACR) instead of in-cluster Zot:
imagePipeline:
  zot:
    enabled: false
  registryHost: "<acct>.dkr.ecr.us-east-1.amazonaws.com"
  buildkitd:
    zotPlaintext: false
    credHelpers:
      "<acct>.dkr.ecr.us-east-1.amazonaws.com": ecr-login   # or: gcr / acr-env

# Cloud identity for the build + task-run pods (cloud-neutral annotation map):
serviceAccount:
  controllerAnnotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::<acct>:role/djinn      # EKS IRSA
    # iam.gke.io/gcp-service-account: <sa>@<proj>.iam.gserviceaccount.com  # GKE
    # azure.workload.identity/client-id: <uuid>                            # AKS
  taskrunAnnotations: {}

ingress:
  enabled: true
  className: alb            # or nginx / traefik / istio
  host: djinn.example.com
  tls:
    enabled: true
    secretName: djinn-tls
env:
  publicUrl: https://djinn.example.com
```

Production considerations:

- **Dedicated NodePool** for task-run + warm pods (they must co-locate so warm
  caches are adopted): set `resources.taskrun.nodeSelector` + `tolerations` to
  target it. Multi-node clusters need **RWX** storage for the `mirrors` (and
  `projects`) PVCs — see the `storage.*` comments.
- **GitOps:** ArgoCD (or Flux) can track `deploy/helm/djinn` directly from this
  repo as an external chart and apply your `values` overlay.
- **Secrets** are best delivered out-of-band (External Secrets Operator,
  Doppler, Vault) into the namespace and referenced via the chart's
  `*.existingSecret` knobs rather than inlined into values.

---

## Values reference

| Setting | Purpose |
|---------|---------|
| `image.server`, `image.runtime` | Server + agent-runtime image refs (pin for prod; tags have **no** `v` prefix). |
| `env.publicUrl` | Public ingress URL — **required** for OAuth callbacks and the MCP audience. |
| `ingress.{enabled,className,host,tls}` | Expose the server externally. |
| `secrets.providers.*` | LLM API keys (`anthropicApiKey`, `openaiApiKey`, …) bootstrapped into the encrypted vault. Other providers (e.g. Fireworks → `FIREWORKS_API_KEY`) can be injected via `extraEnv`. |
| `secrets.githubApp.*` | GitHub App credentials for repo clone/push/PRs. |
| `secrets.vaultKey.key` | Base64 32-byte AES key for the credential vault (auto-generated + preserved across upgrades if empty). |
| `postgres.enabled` / `database.{externalUrl,existingSecret}` | Bundled Postgres vs. external (RDS / Cloud SQL). |
| `qdrant.enabled` | Bundled Qdrant vs. external. |
| `imagePipeline.{registryHost,buildkitd,zot}` | Per-project image build + registry (in-cluster Zot, or ECR/GCR/ACR via cred helpers). |
| `serviceAccount.{controller,taskrun}Annotations` | Cloud identity (IRSA / Workload Identity / AKS) for build + task-run pods. |
| `resources.taskrun.{nodeSelector,tolerations}` | Pin task-run + warm pods to a dedicated NodePool. |
| `langfuse.{enabled,endpoint,publicKey,secretKey}` | Optional LLM tracing export. |
| `storage.{mirrors,cache,projects}` | PVC sizes / access modes / storage classes (RWO single-node, RWX multi-node). |

## Upgrades

```bash
helm upgrade djinn deploy/helm/djinn -n djinn -f my-values.yaml
```

The migrate initContainer applies any new migrations before the new server pod
serves traffic; the rolling update keeps the old pod up until the new one is
ready. Bump `image.server` + `image.runtime` together. The vault key and
auto-generated passwords are preserved across upgrades.
