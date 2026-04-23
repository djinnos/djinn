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
#   - port-forwards :3000 (API/UI), :8443 (worker RPC), :3306 (Dolt),
#     :6333/:6334 (Qdrant), :5000 (Langfuse dashboard), and :9091 (MinIO
#     console) so no manual kubectl port-forward terminals.
#
# `tilt down` deletes the Helm release but leaves the kind cluster + registry
# alive. To delete the cluster: `kind delete cluster --name djinn`.

CLUSTER  = 'kind-djinn'
NS       = 'djinn'
REGISTRY = 'localhost:5001'
# The chart renders `kind-registry:5000` refs so in-cluster pulls resolve via
# the kind node's /etc/hosts alias. Host-side pushes go through localhost:5001,
# which is the same registry from the host's vantage point.
IN_CLUSTER_REGISTRY = 'kind-registry:5000'

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
watch_file('.tilt/artifacts/djinn-agent-worker')
watch_file('server/docker/djinn-image-builder.Dockerfile')

def _content_tag(path):
    if not os.path.exists(path):
        return 'bootstrap'
    md5 = str(local(
        'md5sum "{}" | cut -d" " -f1'.format(path),
        quiet=True,
        echo_off=True,
    )).strip()
    return 'c-{}'.format(md5[:12])

AGENT_RUNTIME_TAG = _content_tag('.tilt/artifacts/djinn-agent-worker')
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
local('bash scripts/kind/setup-kind.sh', quiet=False, echo_off=True)

# Refuse to apply against anything other than the local kind cluster.
allow_k8s_contexts(CLUSTER)

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
)

# --- djinn binaries ------------------------------------------------------
# Host-side cargo build that produces BOTH djinn-server and djinn-agent-
# worker in one pass. They share six workspace crates (djinn-core, djinn-
# db, djinn-graph, djinn-runtime, djinn-supervisor, djinn-workspace) plus
# ~80 external deps unified by workspace-hack, so compiling them together
# cuts per-change work roughly in half versus the old separate-image
# rebuilds. Staged into .tilt/artifacts/; the two wrap-*-image resources
# below pick them up.
#
# BuildKit's cargo target cache-mount was wedging such that source edits
# weren't producing new binaries — named docker volumes (cargo-registry,
# cargo-target, sccache) survive across Tilt invocations without that
# failure mode. The sccache volume also rebuilds the target dir cheaply
# if `docker volume prune` wipes it.
local_resource(
    'djinn-binaries',
    cmd='bash scripts/tilt/build-binaries.sh',
    deps=['server/src', 'server/crates', 'server/Cargo.toml', 'server/Cargo.lock'],
    # Exclude every build artefact dir so `cargo test` on any crate (which
    # writes target/debug/** and target/test-tmp/**) doesn't re-trigger
    # the image build. The workspace has a root `target/` plus per-crate
    # `crates/*/target/` dirs; the `**/target` glob covers both, including
    # future sub-targets. `server/.sqlx` is committed and only changes
    # when the user intentionally runs `cargo sqlx prepare`, so watching
    # it is fine — but the `.../cache` suffix in the old pattern matched
    # nothing.
    ignore=['server/**/target', 'server/**/test-tmp'],
    labels=['build'],
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
# script owns the push to localhost:5001 directly.
custom_build(
    ref='localhost:5001/djinn-server',
    command='bash scripts/tilt/wrap-server-image.sh',
    deps=[
        '.tilt/artifacts/djinn-server',
        'scripts/tilt/wrap-server-image.sh',
    ],
    skips_local_docker=True,
    disable_push=True,
)

# --- djinn-agent-runtime image -------------------------------------------
# Thin wrap on top of djinn-agent-runtime-base: copies in the djinn-agent-
# worker binary and pushes under a content-hashed tag (AGENT_RUNTIME_REF,
# computed above from the artifact md5). The chart plugs this ref into env
# vars the server and controller read at runtime — not into a PodSpec Tilt
# can auto-rewrite — so we route the ref ourselves via `--set` on helm
# template below. `deps` must include the binary artifact so the wrap
# re-runs when djinn-binaries produces a fresh worker; resource_deps alone
# is ordering-only, not a file trigger, so without this line every source
# edit landed in a freshly compiled binary that the next Job never saw.
local_resource(
    'djinn-agent-runtime-image',
    cmd='IMAGE_TAG={ref} bash scripts/tilt/wrap-agent-runtime-image.sh'.format(ref=AGENT_RUNTIME_REF),
    deps=[
        '.tilt/artifacts/djinn-agent-worker',
        'scripts/tilt/wrap-agent-runtime-image.sh',
        'server/docker/djinn-agent-runtime.Dockerfile',
    ],
    resource_deps=['djinn-binaries', 'djinn-agent-runtime-base-image'],
    labels=['build'],
)

# --- djinn-image-builder image ------------------------------------------
# Same reasoning as djinn-agent-runtime: referenced by the controller in
# Job PodSpecs it creates at runtime, not by any chart template. Tag is
# content-hashed from the Dockerfile (IMAGE_BUILDER_REF above) so changes
# to the builder image flow through to a pod roll.
local_resource(
    'djinn-image-builder-image',
    cmd=' && '.join([
        'docker build -f server/docker/djinn-image-builder.Dockerfile -t {ref} .'.format(ref=IMAGE_BUILDER_REF),
        'docker push {ref}'.format(ref=IMAGE_BUILDER_REF),
    ]),
    deps=['server/docker/djinn-image-builder.Dockerfile'],
    labels=['build'],
)

# --- helm override values -------------------------------------------------
# Feed the content-hashed refs into the chart so the server Deployment's
# DJINN_IMAGE_AGENT_WORKER_IMAGE / DJINN_IMAGE_BUILDER_IMAGE env vars pick
# them up. Override AT the helm template call below (not baked into
# values.local.yaml) because the tags change on every worker rebuild.
IMAGE_RUNTIME_SET = 'image.runtime=' + AGENT_RUNTIME_REF_CLUSTER
IMAGE_BUILDER_SET = 'imagePipeline.builderImage=' + IMAGE_BUILDER_REF_CLUSTER

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
    '--set', IMAGE_RUNTIME_SET,
    '--set', IMAGE_BUILDER_SET,
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
    resource_deps=['djinn-binaries', 'djinn-agent-runtime-image'],
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
