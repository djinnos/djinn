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
admin. Wire the GitHub App from the sign-in screen (one-click manifest flow)
or [manually](../GITHUB_APP_SETUP.md).

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
