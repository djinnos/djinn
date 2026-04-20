# -*- mode: Python -*-
#
# djinn local-dev Tilt config.
#
# Replaces the old `make kind-up` / `make image` / `make image-push-local` /
# `make helm-install-local` chain. One command: `tilt up`.
#
# Tilt:
#   - bootstraps the kind cluster + localhost:5001 registry (idempotent),
#   - builds djinn-server on server/** changes and rewrites the Deployment
#     PodSpec to the freshly built tag (so the rollout is automatic),
#   - builds + pushes djinn-agent-runtime under its stable :dev tag (the
#     ConfigMap key DJINN_TASKRUN_IMAGE points at that literal ref; it's
#     consumed by Jobs the controller spawns at runtime, not by a PodSpec
#     Tilt can rewrite),
#   - installs the djinn Helm chart with values.local.yaml,
#   - deploys a self-hosted Langfuse stack (postgres + clickhouse + redis +
#     minio + langfuse-web/worker) that self-seeds a project + API keys via
#     LANGFUSE_INIT_* on first boot, matching the pk/sk values.local.yaml
#     feeds into djinn-server's env,
#   - port-forwards :3000 (API/UI), :8443 (worker RPC), :3306 (Dolt),
#     :6333/:6334 (Qdrant), :5000 (Langfuse dashboard), and :9091 (MinIO
#     console) so no manual kubectl port-forward terminals.
#
# `tilt down` deletes the Helm release but leaves the kind cluster + registry
# alive. To delete the cluster: `kind delete cluster --name djinn`.

CLUSTER  = 'kind-djinn'
NS       = 'djinn'
REGISTRY = 'localhost:5001'
AGENT_RUNTIME_REF = '{}/djinn-agent-runtime:dev'.format(REGISTRY)

# --- kind cluster + registry ---------------------------------------------
# Bootstrap runs at Tiltfile parse (blocking, idempotent) so the cluster
# exists before `allow_k8s_contexts` / `k8s_yaml` try to talk to kubectl.
# Running it as a `local_resource` would defer until after parse and every
# workload would sit in "Waiting for cluster connection".
local('bash scripts/kind/setup-kind.sh', quiet=False, echo_off=True)

# Refuse to apply against anything other than the local kind cluster.
allow_k8s_contexts(CLUSTER)

# --- djinn-server image --------------------------------------------------
# Use the literal localhost:5001 ref so Tilt's image-injection matches the
# Deployment PodSpec's `localhost:5001/djinn-server:dev` unambiguously and
# pushes to the in-cluster registry kind is wired to pull from.
docker_build(
    ref='{}/djinn-server'.format(REGISTRY),
    context='.',
    dockerfile='server/docker/djinn-server.Dockerfile',
    ignore=[
        'server/target',
        'server/.sqlx/cache',
        'ui',
        'deploy',
        '.claude',
        # Do NOT exclude markdown files: several crates (djinn-provider)
        # `include_str!` prompt `.md` files at compile time, and excluding
        # them makes the build fail with "No such file or directory".
    ],
)

# --- djinn-agent-runtime image -------------------------------------------
# Not referenced by any PodSpec at render time — the controller reads
# DJINN_TASKRUN_IMAGE from its ConfigMap and plugs it into Jobs it creates
# at dispatch time. Tilt can't rewrite ConfigMap values, so we build + push
# under the stable :dev tag values.local.yaml already points at.
local_resource(
    'djinn-agent-runtime-image',
    cmd=' && '.join([
        'docker build -f server/docker/djinn-agent-runtime.Dockerfile -t {ref} .'.format(ref=AGENT_RUNTIME_REF),
        'docker push {ref}'.format(ref=AGENT_RUNTIME_REF),
    ]),
    deps=['server', 'server/docker/djinn-agent-runtime.Dockerfile'],
    ignore=['server/target', 'server/.sqlx/cache'],
    labels=['build'],
)

# --- Vault key pinning ---------------------------------------------------
# The chart's secret-vault-key template uses Helm `lookup` to preserve the
# AES key across upgrades. Tilt's `helm()` call runs `helm template`
# client-side, where `lookup` always returns nil — so every reload would
# generate a fresh randBytes(32) and `kubectl apply` would overwrite the
# Secret, leaving any vault-encrypted rows undecryptable. Work around by
# generating a stable dev key into a gitignored file once and passing it
# via --set so the operator-supplied branch wins every render.
local(
    'mkdir -p .tilt && [ -s .tilt/vault.key ] || openssl rand -base64 32 | tr -d "\\n" > .tilt/vault.key',
    quiet=True,
    echo_off=True,
)
VAULT_KEY = str(read_file('.tilt/vault.key')).strip()

# --- GitHub App credentials ---------------------------------------------
# Optional. If `.tilt/github-app/` exists with the four files below, Tilt
# passes them to the chart via --set-file so the chart renders its own
# Secret. Missing files → chart renders a Secret with empty strings and
# the djinn-server Deployment mounts it as optional (pod starts, GitHub
# auth is just disabled). Files are gitignored via /.tilt/.
#
# Expected layout:
#   .tilt/github-app/app-id          — GitHub App numeric ID
#   .tilt/github-app/client-id       — Client ID (Iv1.* / Iv23li*)
#   .tilt/github-app/client-secret   — Client secret
#   .tilt/github-app/private-key.pem — Private key PEM file
GITHUB_APP_FILES = [
    ('secrets.githubApp.appId',        '.tilt/github-app/app-id'),
    ('secrets.githubApp.clientId',     '.tilt/github-app/client-id'),
    ('secrets.githubApp.clientSecret', '.tilt/github-app/client-secret'),
    ('secrets.githubApp.privateKey',   '.tilt/github-app/private-key.pem'),
]
gh_present = [(k, p) for k, p in GITHUB_APP_FILES if os.path.exists(p)]
gh_missing = [p for k, p in GITHUB_APP_FILES if not os.path.exists(p)]
if gh_missing and len(gh_missing) < len(GITHUB_APP_FILES):
    warn('GitHub App credentials partially configured; missing: {}'.format(', '.join(gh_missing)))

# --- Helm release --------------------------------------------------------
# Tilt's native `helm()` doesn't accept `--set-file`, and `--set` mangles
# PEM newlines. Shell out to `helm template` directly so we can pass
# arbitrary flags and feed the raw YAML into k8s_yaml via blob().
# djinn-crds has no templates yet (reserved for future CRDs) — skip until
# it grows real manifests; reinstate with a second helm template call
# when that happens.
helm_cmd = [
    'helm', 'template', 'djinn', 'deploy/helm/djinn',
    '--namespace', NS,
    '--values', 'deploy/helm/djinn/values.local.yaml',
    '--set', 'secrets.vaultKey.key=' + VAULT_KEY,
]
for key, path in gh_present:
    helm_cmd += ['--set-file', '{}={}'.format(key, path)]
k8s_yaml(local(' '.join(helm_cmd), quiet=True, echo_off=True))

# --- Langfuse stack ------------------------------------------------------
# Deploys into the djinn namespace so the djinn-server env can dial
# langfuse-web via short service DNS. First-boot headless init seeds the
# project + pk/sk baked into values.local.yaml — no manual dashboard signup.
k8s_yaml('deploy/langfuse-local/langfuse.yaml')

# --- Workloads + port-forwards ------------------------------------------
k8s_resource(
    workload='djinn-server',
    port_forwards=[
        port_forward(3000, 3000, name='api-ui'),
        port_forward(8443, 8443, name='worker-rpc'),
    ],
    resource_deps=['djinn-agent-runtime-image'],
    labels=['djinn'],
)
k8s_resource(
    workload='djinn-dolt',
    port_forwards=[port_forward(3306, 3306, name='mysql')],
    labels=['infra'],
)
k8s_resource(
    workload='djinn-qdrant',
    port_forwards=[
        port_forward(6333, 6333, name='http'),
        port_forward(6334, 6334, name='grpc'),
    ],
    labels=['infra'],
)

# --- Vite dev server for the React UI -----------------------------------
# Runs on the host (not in-cluster) so HMR works over localhost and pnpm
# caches persist. values.local.yaml's env.webUrl already points djinn-
# server's OAuth redirect at http://localhost:1420, so everything just
# works without Ingress. Installs deps on first boot if node_modules is
# missing; cheap no-op otherwise.
local_resource(
    'djinn-ui',
    cmd='cd ui && [ -d node_modules ] || pnpm install --frozen-lockfile',
    serve_cmd='cd ui && pnpm dev --host',
    serve_env={'VITE_DJINN_SERVER_URL': 'http://localhost:3000'},
    readiness_probe=probe(
        period_secs=5,
        http_get=http_get_action(port=1420, path='/'),
    ),
    links=['http://localhost:1420'],
    resource_deps=['djinn-server'],
    labels=['djinn'],
)

# Langfuse: only the web UI + MinIO console are useful on the host. The
# other pods (postgres, clickhouse, redis, worker) stay in-cluster.
k8s_resource(
    workload='langfuse-web',
    port_forwards=[port_forward(5000, 3000, name='dashboard')],
    labels=['langfuse'],
)
k8s_resource(workload='langfuse-worker',     labels=['langfuse'])
k8s_resource(workload='langfuse-postgres',   labels=['langfuse'])
k8s_resource(workload='langfuse-clickhouse', labels=['langfuse'])
k8s_resource(workload='langfuse-redis',      labels=['langfuse'])
k8s_resource(
    workload='langfuse-minio',
    port_forwards=[port_forward(9091, 9001, name='minio-console')],
    labels=['langfuse'],
)
