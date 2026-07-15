# -*- mode: Python -*-
#
# djinn local-dev Tilt config.
#
# Replaces the old `make kind-up` / `make image` / `make image-push-local` /
# `make helm-install-local` chain. One command: `tilt up`.
#
# Tilt:
#   - bootstraps the kind cluster + host-local registry (idempotent),
#   - builds djinn-server on server/** changes and rewrites the Deployment
#     PodSpec to the freshly built tag (so the rollout is automatic),
#   - builds + pushes djinn-agent-runtime under a content-hashed tag (the
#     ref flows into DJINN_TASKRUN_IMAGE and DJINN_IMAGE_AGENT_WORKER_IMAGE
#     via `--set` on helm template below; per-content tag forces BuildKit
#     to invalidate the `COPY --from=…agent-runtime:…` layer when the
#     worker binary changes),
#   - installs the djinn Helm chart with values.local.yaml,
#   - deploys a self-hosted Langfuse stack (postgres + clickhouse + redis +
#     minio + langfuse-web/worker) that self-seeds a project + API keys via
#     LANGFUSE_INIT_* on first boot, matching the pk/sk values.local.yaml
#     feeds into djinn-server's env,
#   - port-forwards :3000 (API/UI), :8443 (worker RPC), :5432 (Postgres),
#     :6333/:6334 (Qdrant), :5000 (Langfuse dashboard), and :9091 (MinIO
#     console) so no manual kubectl port-forward terminals.
#
# `tilt down` deletes the Helm release but leaves the kind cluster + registry
# alive. To delete the cluster: `kind delete cluster --name djinn`.

# For an isolated validation instance, bootstrap a distinct kind context first,
# then pass matching Tiltfile arguments (and a distinct Tilt UI `--port`):
#
#   CLUSTER_NAME=djinn-validation REG_NAME=kind-registry-validation \
#     REG_PORT=15001 bash scripts/kind/setup-kind.sh
#   tilt up --context kind-djinn-validation --port 11350 -- \
#     --cluster-name djinn-validation \
#     --registry-name kind-registry-validation --registry-port 15001 \
#     --state-dir /var/tmp/djinn-tilt-validation \
#     --api-port 13000 --rpc-port 18443 --postgres-port 15432 \
#     --qdrant-http-port 16333 --qdrant-grpc-port 16334 \
#     --langfuse-port 15000 --minio-port 19091
#
# Bare `tilt up` keeps every historical default.

config.define_string('cluster-name', usage='kind cluster name without the kind- prefix')
config.define_string('registry-name', usage='Docker container/DNS name of the kind-local registry')
config.define_string('registry-port', usage='host port published for the kind-local registry')
config.define_string('state-dir', usage='host directory for Tilt artifacts, vault key, and optional GitHub App files')
config.define_string('api-port', usage='host port for the Djinn API/UI')
config.define_string('rpc-port', usage='host port for worker RPC')
config.define_string('postgres-port', usage='host port for Djinn Postgres')
config.define_string('qdrant-http-port', usage='host port for Qdrant HTTP')
config.define_string('qdrant-grpc-port', usage='host port for Qdrant gRPC')
config.define_string('langfuse-port', usage='host port for the Langfuse dashboard')
config.define_string('minio-port', usage='host port for the MinIO console')
config.define_bool('bootstrap-cluster', usage='run the idempotent kind/registry bootstrap during Tiltfile evaluation')
cfg = config.parse()

def _cfg_string(name, default):
    value = cfg.get(name, default)
    if not value:
        fail('--{} must not be empty'.format(name))
    return value

def _cfg_port(name, default):
    value = int(cfg.get(name, str(default)))
    if value < 1 or value > 65535:
        fail('--{} must be between 1 and 65535'.format(name))
    return value

KIND_CLUSTER_NAME = _cfg_string('cluster-name', 'djinn')
CLUSTER           = 'kind-' + KIND_CLUSTER_NAME
NS                = 'djinn'
REGISTRY_NAME     = _cfg_string('registry-name', 'kind-registry')
REGISTRY_PORT     = _cfg_port('registry-port', 5001)
STATE_DIR         = os.path.abspath(_cfg_string('state-dir', '.tilt'))
ARTIFACTS_DIR     = os.path.join(STATE_DIR, 'artifacts')

# Isolated kind instances must not write concurrently into the same Cargo
# target or sccache volumes. Preserve the historical names for bare `tilt up`
# and suffix every non-default cluster's caches with its isolation identity.
CACHE_VOLUME_SUFFIX = ''
if KIND_CLUSTER_NAME != 'djinn':
    CACHE_VOLUME_SUFFIX = '-' + KIND_CLUSTER_NAME
CARGO_REGISTRY_VOLUME = 'djinn-cargo-registry' + CACHE_VOLUME_SUFFIX
CARGO_TARGET_VOLUME = 'djinn-cargo-target' + CACHE_VOLUME_SUFFIX
SCCACHE_VOLUME = 'djinn-sccache' + CACHE_VOLUME_SUFFIX
AGENT_RUNTIME_BASE_IMAGE = 'djinn-agent-runtime-base:dev'
if KIND_CLUSTER_NAME != 'djinn':
    AGENT_RUNTIME_BASE_IMAGE = 'djinn-agent-runtime-base:' + KIND_CLUSTER_NAME

API_HOST_PORT         = _cfg_port('api-port', 3000)
RPC_HOST_PORT         = _cfg_port('rpc-port', 8443)
POSTGRES_HOST_PORT    = _cfg_port('postgres-port', 5432)
QDRANT_HTTP_HOST_PORT = _cfg_port('qdrant-http-port', 6333)
QDRANT_GRPC_HOST_PORT = _cfg_port('qdrant-grpc-port', 6334)
LANGFUSE_HOST_PORT    = _cfg_port('langfuse-port', 5000)
MINIO_HOST_PORT       = _cfg_port('minio-port', 9091)
BOOTSTRAP_CLUSTER     = cfg.get('bootstrap-cluster', True)

host_ports = [
    ('registry-port', REGISTRY_PORT),
    ('api-port', API_HOST_PORT),
    ('rpc-port', RPC_HOST_PORT),
    ('postgres-port', POSTGRES_HOST_PORT),
    ('qdrant-http-port', QDRANT_HTTP_HOST_PORT),
    ('qdrant-grpc-port', QDRANT_GRPC_HOST_PORT),
    ('langfuse-port', LANGFUSE_HOST_PORT),
    ('minio-port', MINIO_HOST_PORT),
]
seen_host_ports = {}
for name, value in host_ports:
    previous = seen_host_ports.get(value)
    if previous:
        fail('--{} and --{} both use host port {}'.format(previous, name, value))
    seen_host_ports[value] = name

REGISTRY = 'localhost:{}'.format(REGISTRY_PORT)
# The chart renders `<registry-name>:5000` refs so in-cluster pulls resolve via
# Docker DNS. Host-side pushes go through the configured localhost port, which
# is the same registry from the host's vantage point.
IN_CLUSTER_REGISTRY = '{}:5000'.format(REGISTRY_NAME)
PUBLIC_URL = 'http://localhost:{}'.format(API_HOST_PORT)

# Content-hashed tags for the agent-runtime + image-builder images.
#
# These images aren't referenced by any PodSpec image field Tilt can rewrite
# — they land as env vars the server Pod reads and the image-controller
# threads into `compute_environment_hash` as `agent_worker_ref` / as the
# build-Pod's image. With a stable `:dev` tag BuildKit's remote layer cache
# reuses the prior `COPY --from=…-runtime:dev` layer even when the underlying
# worker binary changed (cache key = source image digest, but Tilt never
# invalidated the tag → BuildKit pulled the prior digest from the cache
# manifest). Moving to a per-content tag forces a fresh digest on every worker
# rebuild, which cascades through: wrap script pushes the new tag → helm
# renders the new value → server pod rolls → next project-image hash differs
# → project images rebuild with the fresh worker.
#
# `watch_file` re-parses the Tiltfile when the artifact changes so the tag
# here re-computes on every rebuild without a manual Tilt restart.
AGENT_WORKER_ARTIFACT = os.path.join(ARTIFACTS_DIR, 'djinn-agent-worker')
SERVER_ARTIFACT = os.path.join(ARTIFACTS_DIR, 'djinn-server')
watch_file(AGENT_WORKER_ARTIFACT)
watch_file('server/docker/djinn-image-builder.Dockerfile')

def _content_tag(path):
    if not os.path.exists(path):
        return 'bootstrap'
    digest_output = str(local(
        ['openssl', 'dgst', '-sha256', path],
        quiet=True,
        echo_off=True,
    )).strip()
    digest = digest_output.split()[-1]
    return 'c-{}'.format(digest[:12])

AGENT_RUNTIME_TAG = _content_tag(AGENT_WORKER_ARTIFACT)
IMAGE_BUILDER_TAG = _content_tag('server/docker/djinn-image-builder.Dockerfile')

# Host-side refs (what wrap scripts push to).
AGENT_RUNTIME_REF = '{}/djinn-agent-runtime:{}'.format(REGISTRY, AGENT_RUNTIME_TAG)
IMAGE_BUILDER_REF = '{}/djinn-image-builder:{}'.format(REGISTRY, IMAGE_BUILDER_TAG)
# In-cluster refs (what the chart values reference — same image, different
# network vantage point).
AGENT_RUNTIME_REF_CLUSTER = '{}/djinn-agent-runtime:{}'.format(IN_CLUSTER_REGISTRY, AGENT_RUNTIME_TAG)
IMAGE_BUILDER_REF_CLUSTER = '{}/djinn-image-builder:{}'.format(IN_CLUSTER_REGISTRY, IMAGE_BUILDER_TAG)

# --- kind cluster + registry ---------------------------------------------
# Bootstrap runs at Tiltfile parse (blocking, idempotent) so the cluster
# exists before `allow_k8s_contexts` / `k8s_yaml` try to talk to kubectl.
# Running it as a `local_resource` would defer until after parse and every
# workload would sit in "Waiting for cluster connection".
if BOOTSTRAP_CLUSTER:
    local(
        ['bash', 'scripts/kind/setup-kind.sh'],
        quiet=False,
        echo_off=True,
        env={
            'CLUSTER_NAME': KIND_CLUSTER_NAME,
            'REG_NAME': REGISTRY_NAME,
            'REG_PORT': str(REGISTRY_PORT),
        },
    )

# Refuse to apply against anything other than the local kind cluster.
allow_k8s_contexts(CLUSTER)

# Only the slow recompile/bundle steps need to be manual — the cheap wrap
# steps below (djinn-server image, djinn-agent-runtime image) stay AUTO so
# they cascade-roll the pods as soon as djinn-binaries finishes. Hit the
# refresh arrow on djinn-binaries (or `tilt trigger djinn-binaries`) when
# you want a fresh compile; the rest follows automatically.

# --- djinn-agent-runtime base image --------------------------------------
# Heavy base: LSPs (Node + rust-analyzer + pyright + typescript-language-
# server), rustup + stable toolchain, sccache + mold + clang, non-root
# user. Rebuilt only when its Dockerfile changes (tarball version bumps,
# apt dep edits). Tagged locally — never pushed; the top wrap step resolves
# the FROM against the local docker image store. Keeping LSP fetches + apt
# out of the per-build path is the single biggest layering win: worker
# source edits no longer bust 1.5 GB of LSP downloads.
local_resource(
    'djinn-agent-runtime-base-image',
    cmd='bash scripts/tilt/build-agent-runtime-base.sh',
    deps=['server/docker/djinn-agent-runtime-base.Dockerfile'],
    labels=['build'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    env={'BASE_TAG': AGENT_RUNTIME_BASE_IMAGE},
)

# --- djinn UI (Vite production build, embedded into djinn-server) -------
# djinn-server embeds `ui/dist/` via rust-embed at compile time, so this
# must run before `djinn-binaries` or cargo will embed a stale (or
# placeholder) UI. Re-fires on any UI source change, which cascades into
# a server rebuild via resource_deps below.
#
# All dev + prod traffic goes to :3000 (single origin, same as shipping
# image). The previous :1420 Vite HMR shortcut was removed — per-origin
# localStorage caused UI drift between the two URLs. UI edits now cost a
# `pnpm build` + server rebuild + pod roll cycle.
local_resource(
    'djinn-ui-dist',
    cmd=['bash', 'scripts/tilt/build-ui.sh'],
    deps=[
        'ui/src',
        'ui/public',
        'ui/index.html',
        'ui/.npmrc',
        'ui/package.json',
        'ui/pnpm-lock.yaml',
        'ui/pnpm-workspace.yaml',
        'ui/vite.config.ts',
        'ui/tsconfig.json',
        'ui/tsconfig.app.json',
        'ui/tsconfig.node.json',
        'scripts/tilt/build-ui.sh',
        'scripts/tilt/input-fingerprint.sh',
    ],
    ignore=['ui/dist', 'ui/node_modules', 'ui/storybook-static'],
    labels=['build'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    env={'ARTIFACTS_DIR': ARTIFACTS_DIR},
)

# --- djinn binaries ------------------------------------------------------
# Host-side cargo build that produces BOTH djinn-server and djinn-agent-
# worker in one pass. They share six workspace crates (djinn-core, djinn-
# db, djinn-graph, djinn-runtime, djinn-supervisor, djinn-workspace) plus
# ~80 external deps unified by workspace-hack, so compiling them together
# cuts per-change work roughly in half versus the old separate-image
# rebuilds. Staged into the configured state-dir's artifacts/; the two
# wrap-*-image resources
# below pick them up.
#
# BuildKit's cargo target cache-mount was wedging such that source edits
# weren't producing new binaries — named docker volumes (cargo-registry,
# cargo-target, sccache) survive across Tilt invocations without that
# failure mode. The sccache volume also rebuilds the target dir cheaply
# if `docker volume prune` wipes it.
local_resource(
    'djinn-binaries',
    cmd=['bash', 'scripts/tilt/build-binaries.sh'],
    deps=[
        'server/src',
        'server/crates',
        'server/.cargo',
        'server/.sqlx',
        'server/Cargo.toml',
        'server/Cargo.lock',
        'server/rust-toolchain.toml',
        'server/build.rs',
        'server/docker/djinn-binaries-builder.Dockerfile',
        'scripts/tilt/build-binaries.sh',
        'scripts/tilt/build-ui.sh',
        'scripts/tilt/input-fingerprint.sh',
        'ui/dist',
    ],
    # Exclude every build artefact dir so `cargo test` on any crate (which
    # writes target/debug/** and target/test-tmp/**) doesn't re-trigger
    # the image build. The workspace has a root `target/` plus per-crate
    # `crates/*/target/` dirs; the `**/target` glob covers both, including
    # future sub-targets. `server/.sqlx` is committed and only changes
    # when the user intentionally runs `cargo sqlx prepare`, so watching
    # it is fine — but the `.../cache` suffix in the old pattern matched
    # nothing.
    ignore=['server/**/target', 'server/**/test-tmp'],
    resource_deps=['djinn-ui-dist'],
    labels=['build'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    env={
        'ARTIFACTS_DIR': ARTIFACTS_DIR,
        'CARGO_REGISTRY_VOLUME': CARGO_REGISTRY_VOLUME,
        'TARGET_VOLUME': CARGO_TARGET_VOLUME,
        'SCCACHE_VOLUME': SCCACHE_VOLUME,
    },
)

# --- djinn-server image --------------------------------------------------
# Thin wrap: debian-slim + the freshly-built djinn-server binary + tini.
# `custom_build` (vs. `local_resource` + stable :dev tag) is what makes the
# pod actually roll on binary changes: Tilt generates a per-build $EXPECTED_REF,
# the wrap script builds + pushes under that tag, and Tilt rewrites the
# Deployment PodSpec to point at it — so K8s sees a new image ref and rolls.
# With a stable :dev tag + `local_resource`, docker push would update the
# registry digest but the PodSpec field would be unchanged, so the running
# pod kept the stale binary (cf. the "missing field project" MCP error in
# the Proposals UI on 2026-04-22). `skips_local_docker=True` because the
# script owns the push to the configured host registry directly.
custom_build(
    ref='{}/djinn-server'.format(REGISTRY),
    command=['bash', 'scripts/tilt/wrap-server-image.sh'],
    deps=[
        SERVER_ARTIFACT,
        'scripts/tilt/wrap-server-image.sh',
        'server/docker/djinn-server.Dockerfile',
    ],
    skips_local_docker=True,
    disable_push=True,
    env={'ARTIFACTS_DIR': ARTIFACTS_DIR},
)

# --- djinn-agent-runtime image -------------------------------------------
# Thin wrap on top of djinn-agent-runtime-base: copies in the djinn-agent-
# worker binary and pushes under a content-hashed tag (AGENT_RUNTIME_REF,
# computed above from the artifact SHA-256). The chart plugs this ref into env
# vars the server and controller read at runtime — not into a PodSpec Tilt
# can auto-rewrite — so we route the ref ourselves via `--set` on helm
# template below. `deps` must include the binary artifact so the wrap
# re-runs when djinn-binaries produces a fresh worker; resource_deps alone
# is ordering-only, not a file trigger, so without this line every source
# edit landed in a freshly compiled binary that the next Job never saw.
local_resource(
    'djinn-agent-runtime-image',
    cmd=['bash', 'scripts/tilt/wrap-agent-runtime-image.sh'],
    deps=[
        AGENT_WORKER_ARTIFACT,
        'scripts/tilt/wrap-agent-runtime-image.sh',
        'server/docker/djinn-agent-runtime.Dockerfile',
    ],
    resource_deps=['djinn-binaries', 'djinn-agent-runtime-base-image'],
    labels=['build'],
    env={
        'ARTIFACTS_DIR': ARTIFACTS_DIR,
        'IMAGE_TAG': AGENT_RUNTIME_REF,
        'BASE_IMAGE': AGENT_RUNTIME_BASE_IMAGE,
    },
)

# --- djinn-image-builder image ------------------------------------------
# Same reasoning as djinn-agent-runtime: referenced by the controller in
# Job PodSpecs it creates at runtime, not by any chart template. Tag is
# content-hashed from the Dockerfile (IMAGE_BUILDER_REF above) so changes
# to the builder image flow through to a pod roll.
local_resource(
    'djinn-image-builder-image',
    cmd=' && '.join([
        'docker build -f server/docker/djinn-image-builder.Dockerfile -t {ref} .'.format(ref=shlex.quote(IMAGE_BUILDER_REF)),
        'docker push {ref}'.format(ref=shlex.quote(IMAGE_BUILDER_REF)),
    ]),
    deps=['server/docker/djinn-image-builder.Dockerfile'],
    labels=['build'],
    trigger_mode=TRIGGER_MODE_MANUAL,
)

# --- helm override values -------------------------------------------------
# Feed the content-hashed refs into the chart so the server Deployment's
# DJINN_IMAGE_AGENT_WORKER_IMAGE / DJINN_IMAGE_BUILDER_IMAGE env vars pick
# them up. Override AT the helm template call below (not baked into
# values.local.yaml) because the tags change on every worker rebuild.
IMAGE_RUNTIME_SET = 'image.runtime=' + AGENT_RUNTIME_REF_CLUSTER
IMAGE_BUILDER_SET = 'imagePipeline.builderImage=' + IMAGE_BUILDER_REF_CLUSTER
IMAGE_SERVER_SET = 'image.server={}/djinn-server:dev'.format(REGISTRY)
IMAGE_REGISTRY_HOST_SET = 'imagePipeline.registryHost=' + IN_CLUSTER_REGISTRY
IMAGE_INSECURE_REGISTRY_SET = 'imagePipeline.buildkitd.insecureRegistries[0]=' + IN_CLUSTER_REGISTRY
PUBLIC_URL_SET = 'env.publicUrl=' + PUBLIC_URL
WEB_URL_SET = 'env.webUrl=' + PUBLIC_URL

# --- Vault key pinning ---------------------------------------------------
# The chart's secret-vault-key template uses Helm `lookup` to preserve the
# AES key across upgrades. Tilt's `helm()` call runs `helm template`
# client-side, where `lookup` always returns nil — so every reload would
# generate a fresh randBytes(32) and `kubectl apply` would overwrite the
# Secret, leaving any vault-encrypted rows undecryptable. Work around by
# generating a stable dev key into a gitignored file once and passing it
# via --set so the operator-supplied branch wins every render.
local(
    [
        'bash',
        '-c',
        ''.join([
            'set -euo pipefail; mkdir -p "$STATE_DIR"; umask 077; ',
            'if [ ! -s "$STATE_DIR/vault.key" ]; then ',
            'openssl rand -base64 32 | tr -d "\\n" > "$STATE_DIR/vault.key"; fi; ',
            'chmod 600 "$STATE_DIR/vault.key"',
        ]),
    ],
    quiet=True,
    echo_off=True,
    env={'STATE_DIR': STATE_DIR},
)
VAULT_KEY_PATH = os.path.join(STATE_DIR, 'vault.key')
VAULT_KEY = str(read_file(VAULT_KEY_PATH)).strip()

# --- GitHub App credentials ---------------------------------------------
# Optional. If `<state-dir>/github-app/` exists with the four files below, Tilt
# passes them to the chart via --set-file so the chart renders its own
# Secret. With no files, the chart omits the Secret/env/path surface entirely
# so the local manifest self-setup path can persist and hot-load credentials.
# A partial file set still renders the attempted configuration and remains a
# fatal server-side error. The default `.tilt/` state directory is gitignored;
# keep custom state directories outside the repository.
#
# Expected layout:
#   <state-dir>/github-app/app-id          — GitHub App numeric ID
#   <state-dir>/github-app/client-id       — Client ID (Iv1.* / Iv23li*)
#   <state-dir>/github-app/client-secret   — Client secret
#   <state-dir>/github-app/private-key.pem — Private key PEM file
GITHUB_APP_DIR = os.path.join(STATE_DIR, 'github-app')
GITHUB_APP_FILES = [
    ('secrets.githubApp.appId',        os.path.join(GITHUB_APP_DIR, 'app-id')),
    ('secrets.githubApp.clientId',     os.path.join(GITHUB_APP_DIR, 'client-id')),
    ('secrets.githubApp.clientSecret', os.path.join(GITHUB_APP_DIR, 'client-secret')),
    ('secrets.githubApp.privateKey',   os.path.join(GITHUB_APP_DIR, 'private-key.pem')),
]
gh_present = [(k, p) for k, p in GITHUB_APP_FILES if os.path.exists(p)]
gh_missing = [p for k, p in GITHUB_APP_FILES if not os.path.exists(p)]
if gh_missing and len(gh_missing) < len(GITHUB_APP_FILES):
    warn('GitHub App credentials partially configured; missing: {}'.format(', '.join(gh_missing)))

# --- Helm release --------------------------------------------------------
# Tilt's native `helm()` doesn't accept `--set-file`, and `--set` mangles
# PEM newlines. Invoke `helm template` directly as an argv list so arbitrary
# flags remain supported without shell interpolation.
# djinn-crds has no templates yet (reserved for future CRDs) — skip until
# it grows real manifests; reinstate with a second helm template call
# when that happens.
LOCAL_VALUES_PATH = 'deploy/helm/djinn/values.local.yaml'
# `local(helm_cmd)` cannot infer file dependencies from argv. Register the
# values file explicitly so resource/capacity edits re-render the chart.
read_file(LOCAL_VALUES_PATH)
helm_cmd = [
    'helm', 'template', 'djinn', 'deploy/helm/djinn',
    '--namespace', NS,
    '--values', LOCAL_VALUES_PATH,
    '--set-string', 'secrets.vaultKey.key=' + VAULT_KEY,
    '--set-string', IMAGE_RUNTIME_SET,
    '--set-string', IMAGE_BUILDER_SET,
    '--set-string', IMAGE_SERVER_SET,
    '--set-string', IMAGE_REGISTRY_HOST_SET,
    '--set-string', IMAGE_INSECURE_REGISTRY_SET,
    '--set-string', PUBLIC_URL_SET,
    '--set-string', WEB_URL_SET,
]
for key, path in gh_present:
    helm_cmd += ['--set-file', '{}={}'.format(key, path)]
k8s_yaml(local(helm_cmd, quiet=True, echo_off=True))

# --- Langfuse stack ------------------------------------------------------
# Deploys into the djinn namespace so the djinn-server env can dial
# langfuse-web via short service DNS. First-boot headless init seeds the
# project + pk/sk baked into values.local.yaml — no manual dashboard signup.
# NEXTAUTH_URL must follow an overridden dashboard host port in isolated runs.
langfuse_yaml = str(read_file('deploy/langfuse-local/langfuse.yaml')).replace(
    'value: http://localhost:5000',
    'value: http://localhost:{}'.format(LANGFUSE_HOST_PORT))
k8s_yaml(blob(langfuse_yaml))

# --- Workloads + port-forwards ------------------------------------------
k8s_resource(
    workload='djinn-server',
    port_forwards=[
        port_forward(API_HOST_PORT, 3000, name='api-ui'),
        port_forward(RPC_HOST_PORT, 8443, name='worker-rpc'),
    ],
    resource_deps=['djinn-binaries', 'djinn-agent-runtime-image'],
    labels=['djinn'],
)
k8s_resource(
    workload='djinn-postgres',
    port_forwards=[port_forward(POSTGRES_HOST_PORT, 5432, name='postgres')],
    labels=['infra'],
)
k8s_resource(
    workload='djinn-qdrant',
    port_forwards=[
        port_forward(QDRANT_HTTP_HOST_PORT, 6333, name='http'),
        port_forward(QDRANT_GRPC_HOST_PORT, 6334, name='grpc'),
    ],
    labels=['infra'],
)

# Langfuse: only the web UI + MinIO console are useful on the host. The
# other pods (postgres, clickhouse, redis, worker) stay in-cluster.
k8s_resource(
    workload='langfuse-web',
    port_forwards=[port_forward(LANGFUSE_HOST_PORT, 3000, name='dashboard')],
    labels=['langfuse'],
)
k8s_resource(workload='langfuse-worker',     labels=['langfuse'])
k8s_resource(workload='langfuse-postgres',   labels=['langfuse'])
k8s_resource(workload='langfuse-clickhouse', labels=['langfuse'])
k8s_resource(workload='langfuse-redis',      labels=['langfuse'])
k8s_resource(
    workload='langfuse-minio',
    port_forwards=[port_forward(MINIO_HOST_PORT, 9001, name='minio-console')],
    labels=['langfuse'],
)
