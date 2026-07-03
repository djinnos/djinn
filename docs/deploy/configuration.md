# Configuration reference

Everything the chart lets you change, grouped by concern. Defaults live in
[`deploy/helm/djinn/values.yaml`](../../deploy/helm/djinn/values.yaml) — the
comments there are authoritative; this page is the map.

- [Images](#images)
- [URL, ingress, TLS](#url-ingress-tls)
- [Database (bundled Postgres vs RDS / Cloud SQL)](#database)
- [Vector store (Qdrant)](#vector-store-qdrant)
- [Image pipeline & registry](#image-pipeline--registry)
- [Storage](#storage)
- [Secrets](#secrets)
- [LLM providers](#llm-providers)
- [Resources & scheduling](#resources--scheduling)
- [Observability (Langfuse)](#observability-langfuse)
- [Extra env](#extra-env)
- [Users & admin](#users--admin)
- [Operations](#operations)

## Images

```yaml
image:
  server: ghcr.io/djinnos/djinn-server:0.6.69        # control plane
  runtime: ghcr.io/djinnos/djinn-agent-runtime:0.6.69 # task-run pods
```

Tags carry **no leading `v`**. Pin both to the same release and bump them
together. The buildkitd/builder images default to `:latest` and can be pinned
via `imagePipeline.buildkitd.image` / `imagePipeline.builderImage`.

## URL, ingress, TLS

```yaml
env:
  publicUrl: https://djinn.example.com   # REQUIRED for GitHub OAuth callbacks + MCP audience
ingress:
  enabled: true
  className: traefik                     # or alb / nginx / istio
  host: djinn.example.com
  annotations: {}                        # e.g. cert-manager.io/cluster-issuer
  tls: { enabled: true, secretName: djinn-tls }
```

`env.publicUrl` must match what browsers actually hit — GitHub login, the
one-click GitHub App creation, and MCP OAuth all redirect through it.

## Database

All state — proposals, tasks, sessions, users, encrypted credentials, the
code-graph cache — lives in **Postgres 16**.

**Bundled** (default; single-node installs):

```yaml
postgres:
  enabled: true
  auth: { password: "<set-one>" }        # empty = local-dev default, kind only
  storage: { size: 20Gi }
  config: { sharedBuffers: 2GB, maxConnections: 200 }
```

**External — RDS / Cloud SQL / any Postgres 16+.** Disable the bundle and
provide the URL one of two ways (set exactly one):

```yaml
postgres:
  enabled: false
database:
  # Preferred: a Secret with a `url` key. The URL is read via secretKeyRef and
  # never lands in a ConfigMap or `helm get values`.
  existingSecret: djinn-db
  # Or a literal (fine for staging, bakes the password into the release):
  # externalUrl: postgres://djinn:pass@host:5432/djinn?sslmode=require
```

Migrations run automatically either way (initContainer,
[details](README.md#how-installs-and-upgrades-work)).

## Vector store (Qdrant)

```yaml
qdrant:
  enabled: true            # bundled StatefulSet; false = bring your own
```

Qdrant holds code-chunk and note embeddings for semantic/hybrid search.
Postgres remains the source of truth; vectors are rebuildable.

## Image pipeline & registry

Each project gets a devcontainer image, built by a rootless BuildKit daemon
and pushed to an OCI registry that the kubelet then pulls task-run pods from.

**In-cluster registry (Zot)** — self-contained, right for VPS/air-gapped:

```yaml
imagePipeline:
  enabled: true
  buildkitd:
    zotPlaintext: true                # Zot is plain HTTP on a ClusterIP
  zot:
    enabled: true
    anonymousPull: true               # kubelet pulls without a pull-secret
    auth: { username: djinn, password: "<set-one>" }
    storage: { size: 100Gi }
```

On k3s the kubelet also needs a `registries.yaml` mirror entry for the Zot
Service hostname — see [the VPS guide, step 5](vps.md#5-wire-the-kubelet-to-the-in-cluster-registry).

**Managed registry (ECR / GAR / ACR)**:

```yaml
imagePipeline:
  zot: { enabled: false }
  registryHost: "<acct>.dkr.ecr.us-east-1.amazonaws.com"
  buildkitd:
    zotPlaintext: false
    credHelpers:
      "<acct>.dkr.ecr.us-east-1.amazonaws.com": ecr-login   # or: gcr / acr-env
```

Credential-helper binaries are baked into `djinn-buildkitd`; identity comes
from the ServiceAccount annotations:

```yaml
serviceAccount:
  controllerAnnotations:               # cloud-neutral annotation maps
    eks.amazonaws.com/role-arn: arn:aws:iam::<acct>:role/djinn
    # iam.gke.io/gcp-service-account: <sa>@<proj>.iam.gserviceaccount.com
    # azure.workload.identity/client-id: <uuid>
  taskrunAnnotations: {}
```

Other pipeline knobs: `controller.maxConcurrentBuilds` (default 4) caps the
build herd so it can't starve the shared buildkitd; `buildkitd.probes.*`
tunes the generous health-probe defaults; `buildkitd.resources` sizes the
daemon (builds are CPU/memory hungry).

## Storage

Three shared PVCs plus the per-service data volumes:

| PVC | Holds | Access mode |
|-----|-------|-------------|
| `mirrors` | Bare-repo mirrors: server writes, build + task-run Jobs read | RWO single-node, **RWX multi-node** |
| `cache` | cargo/pnpm/pip caches shared across task-runs | same |
| `projects` | Reference working clones | same (RWX on multi-node: the surge rollout briefly runs two server pods) |

```yaml
storage:
  mirrors:  { size: 50Gi, accessMode: ReadWriteMany, storageClassName: "efs", existingClaim: "" }
  cache:    { size: 20Gi, accessMode: ReadWriteMany, storageClassName: "efs" }
  projects: { size: 20Gi, accessMode: ReadWriteMany, storageClassName: "efs" }
```

`accessMode` is immutable on an existing PVC — moving RWO → RWX means
recreating/migrating the claim.

## Secrets

Every secret supports two delivery modes: inline values (rendered into a
chart-managed Secret) or `existingSecret` (you deliver a pre-existing Secret
via ESO / Vault / Doppler and the chart mounts it):

```yaml
secrets:
  githubApp:
    existingSecret: ""       # or inline: appId / privateKey / clientId / clientSecret
  vaultKey:
    existingSecret: ""
    key: ""                  # base64 32-byte AES key for the credential vault
  providers:
    existingSecret: ""       # or inline API keys, see below
```

**The vault key is the one you must not lose.** Provider credentials, GitHub
App config, and user OAuth tokens are encrypted with AES-256-GCM under it.
When left empty the chart generates one on first install and preserves it
across upgrades — back the Secret up. Rotating or losing it makes every stored
credential undecryptable.

The GitHub App can also be created *after* deploy via the sign-in screen's
one-click manifest flow (credentials land in the encrypted vault, no Helm
values involved) — see [GitHub App setup](../GITHUB_APP_SETUP.md).

## LLM providers

Two ways in, freely mixed:

1. **Bootstrap via values** — keys land in the encrypted vault at startup:

   ```yaml
   secrets:
     providers:
       anthropicApiKey: ""
       openaiApiKey: ""
       googleApiKey: ""
       azureOpenaiApiKey: ""
       awsAccessKeyId: ""          # Bedrock
       awsSecretAccessKey: ""
       gcpVertexProjectId: ""      # Vertex
   ```

   Providers without a dedicated field (e.g. Fireworks) can be injected via
   `extraEnv` using the provider's required env var (`FIREWORKS_API_KEY`).

2. **Through the UI** — each user connects their own under
   **Settings → Models**: API keys, or OAuth device-code sign-in for
   ChatGPT/Codex and GitHub Copilot. Per-user credentials are encrypted at
   rest with org-shared fallback.

Model selection precedence is **user → project → global**, from a live
[models.dev](https://models.dev) catalog.

## Resources & scheduling

```yaml
resources:
  server:  { requests: {...}, limits: {...} }
  taskrun:                       # applied to every dispatched task-run Job
    requests: { cpu: "500m", memory: "1Gi" }
    limits:   { cpu: "2",    memory: "4Gi" }
    nodeSelector: {}             # pin task-run + warm pods to a node pool
    tolerations: []
```

Task-run and graph-warm pods **must share a node pool** (warm pods pre-warm
caches that task-runs adopt) — one `nodeSelector`/`tolerations` set covers
both. Big Rust/C++ projects may need higher task-run memory limits; the
limits apply per-Job, so total load = limits × your per-user concurrency caps.

`nodeSelector` / `tolerations` / `affinity` at the top level place the server
Deployment itself.

## Observability (Langfuse)

Every agent session streams OpenTelemetry traces when keys are present;
leave blank to disable:

```yaml
langfuse:
  enabled: true
  endpoint: "https://cloud.langfuse.com/api/public/otel"   # or self-hosted svc
  publicKey: "pk-lf-..."
  secretKey: "sk-lf-..."
  existingSecret: ""       # alternative delivery
```

`deploy/langfuse-local/` has a self-hosted Langfuse for dev clusters.

## Extra env

```yaml
extraEnv:                          # ad-hoc tuning knobs
  - { name: FIREWORKS_API_KEY, value: "..." }
extraEnvFrom:                      # mount a whole env-bag Secret (ESO/Doppler)
  - secretRef: { name: djinn-env }
```

Note Helm **replaces** list values file-over-file — keep the whole `extraEnv`
list in one values file.

## Users & admin

- Login is GitHub OAuth through your GitHub App; there are no local passwords.
- **The first user to sign in becomes the admin** (bootstrap rule: stamped when
  no admin exists yet).
- Admins manage users, can act on a user's behalf, and see admin-only surfaces.
- Each user gets a per-model concurrency cap (`max_sessions`) — this is the
  sole admission control on parallel agents; the slot pool itself is elastic.

## Operations

**Upgrades**: bump `image.server` + `image.runtime` together, then
`helm upgrade djinn deploy/helm/djinn -n djinn -f my-values.yaml`. The migrate
initContainer applies new migrations before the new pod serves traffic; the
rolling update keeps the old pod up until the new one is ready.

**Manual migrations** (debugging):
`kubectl run --rm -it djinn-migrate --image=<server-image> --command -- djinn-server --migrate-only`

**Dispatch-state debug endpoint** — admin-gated JSON snapshot of cooldowns,
slot pool, breaker table, in-flight ledger, and pause state:

```bash
curl --cookie 'djinn_session=<admin-cookie>' https://<host>/debug/dispatch-state
```

**MCP**: the full tool surface is at `POST /mcp` (streamable HTTP, OAuth 2.1;
clients discover the authorization server via `/.well-known/` metadata).
