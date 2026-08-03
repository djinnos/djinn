# Managed Kubernetes (EKS / GKE / AKS / kubeadm)

Same chart as everywhere else — the production difference is that the backing
services move out of the cluster: Postgres becomes RDS / Cloud SQL, the image
registry becomes ECR / GAR / ACR, secrets arrive out-of-band, and the manifests
flow through GitOps. The worked example below is EKS (that's what we run);
the [GKE / AKS mapping](#gke--aks-mapping) table translates the cloud-specific
bits. Nothing in the chart is AWS-specific.

## What changes vs. the bundled profile

| Concern | Bundled (VPS) | Production |
|---------|---------------|------------|
| Postgres | In-cluster StatefulSet | RDS / Cloud SQL: `postgres.enabled: false` + `database.existingSecret` |
| Registry | In-cluster Zot | ECR / GAR / ACR via `imagePipeline.registryHost` + `buildkitd.credHelpers` |
| Registry auth | htpasswd | Cloud identity on the ServiceAccounts (IRSA / Workload Identity) |
| Storage | RWO (`local-path`) | **RWX** for `mirrors`, `cache`, `projects` (EFS / Filestore / Azure Files) |
| Secrets | Helm values | External Secrets Operator / Vault / Doppler + `*.existingSecret` |
| Deploys | `helm upgrade` by hand | ArgoCD / Flux tracking `deploy/helm/djinn` |

## Worked example: EKS

### Prerequisites

- An EKS cluster with an ingress controller (ALB controller, or nginx/traefik).
- An **RDS Postgres 16** instance reachable from the cluster, with a `djinn`
  database and user.
- An **ECR repository** (or registry namespace) for per-project images.
- OIDC provider enabled on the cluster (for IRSA).
- **RWX storage class** — EFS CSI driver with a filesystem, unless you keep
  everything on one node group with RWO (not recommended: the surge rollout
  briefly runs two server pods that may land on different nodes).
- Node prerequisite sysctls for rootless BuildKit on any node that can
  schedule `buildkitd` (see [the hub](README.md#node-prerequisites)); on
  managed node images, set them via node group launch-template user data or a
  privileged DaemonSet.
- **Kueue** — required, not optional. `kueue.enabled` and `kueue.armed` both
  default to `true`, so a stock install renders `kueue.x-k8s.io/v1beta1`
  objects and hands build-Job admission to Kueue. Kubernetes **>= 1.30**
  required. See [the prerequisite release](#cluster-prerequisite-releases)
  below.
- **Nodes that pass the writable-cgroup conformance.** `cgroupWritable.taskRuns`
  is on by default, which assigns `RuntimeClass/djinn-cgroup-writable` to every
  task-run Pod; that class carries `scheduling.nodeSelector:
  djinn.io/cgroup-writable=true`. Run
  `deploy/node/k3s/djinn-cgroup-writable-conformance.sh` on each node that
  should run task-runs — it installs the containerd `runc-cgroupwritable`
  handler, proves the node can delegate a writable cgroup, and applies the
  marker label itself once the node passes. Never apply that label by hand: an
  unproven node that carries it schedules Pods that then cannot get a cgroup
  leaf.

### Cluster prerequisite releases

Cluster-scoped third-party operators are installed as their own Helm releases,
before `djinn`, and Djinn does not own their lifecycle. cert-manager is the
familiar one:

```bash
helm repo add jetstack https://charts.jetstack.io
helm upgrade --install cert-manager jetstack/cert-manager \
  --namespace cert-manager --create-namespace --set crds.enabled=true
```

Kueue follows the same pattern, from a chart in this repository that pins the
upstream OCI chart and applies Djinn's scoping as values. Unlike cert-manager
it is **mandatory at stock values** — install it before `djinn`:

```bash
helm upgrade --install djinn-prereqs deploy/helm/djinn-prereqs \
  --namespace kueue-system --create-namespace --wait
```

That pins Kueue `0.19.0` (`oci://registry.k8s.io/kueue/charts/kueue`) and
requires **Kubernetes >= 1.30** — verified by installing on 1.30.13 and 1.31.0,
and by 1.29.14 rejecting the CRDs. The upstream `Chart.yaml` declares no
`kubeVersion`, so Helm will not enforce it — check `kubectl version` first.

The floor is 1.30 only because Djinn's values disable Kueue's DRA feature
gates. Kueue 0.19 ships `KueueDRAIntegration` on, which needs
`resource.k8s.io/v1` (GA in Kubernetes 1.34) and CrashLoopBackOffs below it.
See [deploy/kueue/README.md](../../deploy/kueue/README.md#minimum-kubernetes-is-130-and-only-because-dra-is-disabled).

Djinn's values set a *positive* `managedJobsNamespaceSelector` matching only
namespaces labelled `djinn.io/kueue-managed=true`, so Kueue captures nothing
until a namespace carries that label. At stock `djinn` values it does:
`kueue.armed: true` labels the `djinn` Namespace and renders every task-run,
warm and SCIP Job `suspend: true`, so Workloads **are** created and Kueue's
complete CPU/memory/Pods vector governs admission. Stock static values retain
`buildPods` as a finite fallback bound. With
`kueue.capacity.contract: vector-v1`, CPU and memory fit each Workload's actual
requests while Pods is the eligible Nodes' real post-reserve Kubernetes ceiling,
not a build-shaped concurrency number. The release is inert only
for a deployment that explicitly sets `kueue.armed: false`. Read
[deploy/kueue/README.md](../../deploy/kueue/README.md) — in particular the
residual-risk section — before changing where that label goes: with the
namespace labelled, every `batch/v1` Job CREATE in it routes through a
`failurePolicy: Fail` webhook, so a Kueue control-plane outage blocks them all.

Do **not** substitute stock upstream Kueue for `djinn-prereqs`. At upstream
defaults the Pod/Deployment/StatefulSet webhooks are `failurePolicy: Fail` with
a selector that covers `djinn`, so an unavailable Kueue controller stops
`djinn-server`, Postgres, Qdrant and task-run Pods alike.
`deploy/kueue/tests/webhook-selectors.sh` asserts Djinn's scoping on the
rendered output.

You cannot skip this and still install at stock values: the topology is
`kueue.x-k8s.io/v1beta1`, so a cluster without Kueue is refused — by
`templates/prereq-guard.yaml`, which consults live API discovery during a real
`helm install` and names the missing prerequisite, rather than by the API
server's bare `no matches for kind "ResourceFlavor"`. `ClusterQueue` and
`LocalQueue` also carry a conversion webhook, so the Kueue controller must be
**Ready**, not merely installed — hence the `--wait` above.

The guard also refuses when no node carries `djinn.io/cgroup-writable=true`
while `cgroupWritable.taskRuns.enabled` is true. It exists because both
failures used to be silent: `helm install` reported success and the deployment
then dispatched zero usable Jobs. It is deliberately inert under `helm
template` and client-side `--dry-run` (Helm's `lookup` sees no cluster there),
and it fails **open** if the credentials cannot list Nodes — an operator with
restricted RBAC gets no guard rather than a false refusal.

Bootstrapping order matters here: the conformance probe Pod names
`RuntimeClass/djinn-cgroup-writable-probe`, which only this chart renders, so a
fresh cluster needs one preparation install before conformance can run:

```bash
helm install djinn deploy/helm/djinn --namespace djinn --create-namespace \
  --set cgroupWritable.taskRuns.enabled=false \
  --set cgroupLauncher.mode=disabled \
  --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1
```

All three move together — the armed launcher requires the task-run
RuntimeClass, and `resize-v2` requires the armed launcher, so
`deployment-server.yaml` refuses any subset. That state renders both
RuntimeClasses, assigns neither to task-runs, and leaves the guard's node check
inapplicable. Run the conformance script on each node, then `helm upgrade` back
to stock values. `deploy/runbooks/cgroup-launcher-rearm.md` is the full staged
order.

For a cluster that will never have these prerequisites (kind, a laptop, a
direct-worker profile), opt out of the whole armed profile explicitly — the
switches only work together:

```bash
helm install djinn deploy/helm/djinn --namespace djinn --create-namespace \
  --set cgroupLauncher.mode=disabled \
  --set cgroupWritable.runtimeClass.enabled=false \
  --set cgroupWritable.taskRuns.enabled=false \
  --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1 \
  --set kueue.enabled=false \
  --set kueue.armed=false
```

`deploy/helm/djinn/values.local.yaml` ships exactly that profile, and Tilt uses
it.

For GitOps, `djinn-prereqs` is a separate Application/HelmRelease with a sync
wave ahead of `djinn` — again, exactly like cert-manager.

### 1. IAM for the image pipeline (IRSA)

The controller ServiceAccount drives image builds that push to ECR; buildkitd
authenticates via the baked-in `ecr-login` credential helper using the pod's
IAM role. Create a role with ECR push/pull on your repos
(`ecr:GetAuthorizationToken`, `ecr:BatchGetImage`, `ecr:PutImage`,
`ecr:InitiateLayerUpload`, `ecr:UploadLayerPart`, `ecr:CompleteLayerUpload`,
`ecr:BatchCheckLayerAvailability`) and bind it to the chart's ServiceAccounts
through the annotation values below.

### 2. Database secret

Keep the RDS URL out of Helm values and ConfigMaps — deliver a Secret with a
`url` key (via External Secrets Operator, or by hand):

```bash
kubectl create secret generic djinn-db -n djinn \
  --from-literal=url='postgres://djinn:<password>@<rds-endpoint>:5432/djinn?sslmode=require'
```

The Deployment reads it via `valueFrom: secretKeyRef`, so the URL never lands
in `helm get values` output.

### 3. Values overlay

```yaml
# values.prod.yaml
image:
  server: ghcr.io/djinnos/djinn-server:0.6.69       # pin; bump both together
  runtime: ghcr.io/djinnos/djinn-agent-runtime:0.6.69

# External Postgres — the bundled StatefulSet doesn't render.
postgres:
  enabled: false
database:
  existingSecret: djinn-db          # Secret with a `url` key

qdrant:
  enabled: true                     # or false + your own Qdrant

# Managed registry instead of in-cluster Zot.
imagePipeline:
  zot:
    enabled: false
  registryHost: "<acct>.dkr.ecr.us-east-1.amazonaws.com"
  buildkitd:
    zotPlaintext: false
    credHelpers:
      "<acct>.dkr.ecr.us-east-1.amazonaws.com": ecr-login

# Cloud identity on the pods that build and run tasks.
serviceAccount:
  controllerAnnotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::<acct>:role/djinn
  taskrunAnnotations: {}

# Multi-node: mirrors/cache/projects are shared between the server and Jobs.
storage:
  mirrors:  { accessMode: ReadWriteMany, storageClassName: efs, size: 50Gi }
  cache:    { accessMode: ReadWriteMany, storageClassName: efs, size: 50Gi }
  projects: { accessMode: ReadWriteMany, storageClassName: efs, size: 20Gi }

# Dedicated capacity for agent workloads (optional but recommended): task-run
# and graph-warm pods MUST share a node pool so warm caches are adopted.
resources:
  taskrun:
    nodeSelector:
      workload-type: djinn
    tolerations:
      - { key: workload-type, operator: Equal, value: djinn, effect: NoSchedule }

ingress:
  enabled: true
  className: alb                    # or nginx / traefik / istio
  host: djinn.example.com
  annotations:
    alb.ingress.kubernetes.io/scheme: internet-facing
    alb.ingress.kubernetes.io/target-type: ip
    alb.ingress.kubernetes.io/certificate-arn: arn:aws:acm:...   # ACM TLS
env:
  publicUrl: https://djinn.example.com

# Secrets via ESO/Vault land as named Secrets; reference, don't inline:
secrets:
  githubApp: { existingSecret: djinn-github-app }
  vaultKey:  { existingSecret: djinn-vault-key }
  providers: { existingSecret: djinn-providers }
```

### 4. Install — by hand or GitOps

By hand, from a clone of this repo:

```bash
helm upgrade --install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace -f values.prod.yaml
```

Or let ArgoCD track the chart path directly:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata: { name: djinn, namespace: argocd }
spec:
  project: default
  source:
    repoURL: https://github.com/djinnos/djinn
    targetRevision: v0.6.69            # git tag; image tags have no `v`
    path: deploy/helm/djinn
    helm:
      valueFiles: [ ../../../values/values.prod.yaml ]   # or an in-repo overlay
  destination: { server: https://kubernetes.default.svc, namespace: djinn }
  syncPolicy:
    syncOptions: [ CreateNamespace=true ]
```

Migrations run automatically in the server pod's migrate initContainer on
every rollout; with `replicas: 1` + `maxSurge: 1` the sqlx advisory lock
serialises any overlap. No hook Jobs, no manual step.

### 5. Verify

```bash
kubectl get pods -n djinn                    # server Running & Ready
kubectl logs deploy/djinn-server -n djinn | head -50   # migrations applied, RPC listening
```

Then open `https://djinn.example.com` — the first GitHub sign-in becomes the
admin. If self-setup is enabled (`env.enableSelfSetup: true`), the server boot
log prints a one-time manifest setup URL to create the GitHub App. For
production (self-setup disabled), provide credentials via `secrets.githubApp.*`
in Helm values — see [GitHub App setup](../GITHUB_APP_SETUP.md).

## GKE / AKS mapping

| Concern | EKS | GKE | AKS |
|---------|-----|-----|-----|
| Postgres | RDS | Cloud SQL | Azure Database for PostgreSQL |
| Registry | ECR + `ecr-login` helper | Artifact Registry + `gcr` helper | ACR + `acr-env` helper |
| Pod identity annotation | `eks.amazonaws.com/role-arn` | `iam.gke.io/gcp-service-account` | `azure.workload.identity/client-id` |
| RWX storage | EFS CSI | Filestore CSI | Azure Files CSI |
| Ingress + TLS | ALB + ACM | GCLB + Google-managed certs | AGIC / nginx + cert-manager |

The `buildkitd.credHelpers` map takes any `<registry-host>: <helper>` pairs —
the helper binaries (`docker-credential-ecr-login`, `-gcr`, `-acr-env`) are
baked into the `djinn-buildkitd` image, and cloud identity flows in through
the ServiceAccount annotations.

## Production checklist

- [ ] `image.server` + `image.runtime` pinned to the same release
- [ ] `database.existingSecret` (never `externalUrl` with an inline password)
- [ ] `secrets.vaultKey` pinned or backed up — losing it orphans every stored credential
- [ ] RWX storage on `mirrors`, `cache`, `projects`
- [ ] buildkitd sysctls on the node group that schedules it
- [ ] Task-run node pool tainted + `resources.taskrun.nodeSelector/tolerations` set
- [ ] `env.publicUrl` matches the ingress host exactly (OAuth callbacks)
- [ ] RDS automated backups on (Postgres is the source of truth; vectors can be rebuilt)
