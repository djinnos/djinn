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

# Refuse to apply against anything other than the local kind cluster.
allow_k8s_contexts(CLUSTER)

# --- kind cluster + registry ---------------------------------------------
local_resource(
    'kind-cluster',
    cmd='bash scripts/kind/setup-kind.sh',
    allow_parallel=False,
    labels=['bootstrap'],
)

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
        '**/*.md',
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
    resource_deps=['kind-cluster'],
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

# --- Helm release --------------------------------------------------------
# djinn-crds has no templates yet (reserved for future CRDs) — skip until
# it grows real manifests; reinstate with a second k8s_yaml(helm(...)) call
# when that happens.
k8s_yaml(helm(
    'deploy/helm/djinn',
    name='djinn',
    namespace=NS,
    values=['deploy/helm/djinn/values.local.yaml'],
    set=['secrets.vaultKey.key=' + VAULT_KEY],
))

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
    resource_deps=['kind-cluster', 'djinn-agent-runtime-image'],
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
