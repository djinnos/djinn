# Devcontainer setup

Djinn runs every task inside a per-project container image that is built
from a [devcontainer.json](https://containers.dev/) spec you commit to
your repository. This is a hard requirement — without a devcontainer,
task dispatch is blocked and the UI surfaces an onboarding banner until
one is present.

The decision is deliberate: ops users who bring real dependencies (LSPs,
framework SDKs, vendored binaries) know what they are doing, and a
silent fallback image would be wrong much more often than right.

## Overview

A devcontainer spec lives at `.devcontainer/devcontainer.json` in your
repo root. Djinn's image controller watches the repo's mirror and, when
either `devcontainer.json` or `devcontainer-lock.json` changes, submits
a build job to the in-cluster BuildKit daemon and pushes the resulting
image to the Djinn-internal Zot registry. Task-run Pods pull from that
registry.

The Djinn agent-worker Feature
(`ghcr.io/djinnos/djinn-agent-worker:1`) is **required**. It installs
our Rust worker binary, LSPs (rust-analyzer, typescript-language-server,
pyright), and the SCIP indexers used by the canonical graph warmer.

Requirements:

- Your base image must be glibc-based (`debian`, `ubuntu`, `fedora`,
  `mcr.microsoft.com/devcontainers/base:*`). Alpine/musl bases are
  currently unsupported because upstream LSP binaries are glibc-linked.
- You must also commit `.devcontainer/devcontainer-lock.json`. This
  pins each Feature's OCI digest so rebuilds are reproducible.

## Minimal Rust example

A single-language Rust project:

```json
{
  "name": "my-rust-service",
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu-22.04",
  "features": {
    "ghcr.io/devcontainers/features/rust:1": { "version": "stable" },
    "ghcr.io/djinnos/djinn-agent-worker:1": {}
  },
  "postCreateCommand": "cargo fetch"
}
```

Commit the file, run the lock command (see below), push, and wait for
the banner to go away.

## Polyglot monorepo example

A Rust backend with a TypeScript + pnpm frontend in a single repo:

```json
{
  "name": "my-platform",
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu-22.04",
  "features": {
    "ghcr.io/devcontainers/features/rust:1": { "version": "stable" },
    "ghcr.io/devcontainers/features/node:1": { "version": "22" },
    "ghcr.io/devcontainers-contrib/features/pnpm:2": {},
    "ghcr.io/djinnos/djinn-agent-worker:1": {}
  },
  "postCreateCommand": "cargo fetch && pnpm install -r"
}
```

Djinn's stack detection will have already populated
`package_managers = ["pnpm", "cargo"]` and `frameworks = [...]` so the
banner's generated starter matches this shape verbatim — copy, commit,
done.

## Python + uv example

`uv` is not yet an upstream devcontainer Feature; install it in
`postCreateCommand`:

```json
{
  "name": "my-python-svc",
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu-22.04",
  "features": {
    "ghcr.io/devcontainers/features/python:1": { "version": "3.12" },
    "ghcr.io/djinnos/djinn-agent-worker:1": {}
  },
  "postCreateCommand": "pip install uv && uv sync"
}
```

The same pattern applies to `poetry` and `pdm` — install via `pipx` in
`postCreateCommand`, then run the tool's normal install command.

## Private registry authentication

If your devcontainer pulls from a private registry (private base image,
paid Feature, internal `ghcr.io` package), Djinn injects credentials at
build time via a Kubernetes Secret mounted into the builder Pod's
`~/.docker/config.json`.

Operators: provision the secret as part of the Helm install:

```yaml
# values.local.yaml
imagePipeline:
  zot:
    auth:
      existingSecret: djinn-zot-auth
  registries:
    - name: ghcr.io
      existingSecret: djinn-ghcr-pull
    - name: my-corp.jfrog.io
      existingSecret: djinn-jfrog-pull
```

Each referenced Secret must contain a
[`.dockerconfigjson`](https://kubernetes.io/docs/tasks/configure-pod-container/pull-image-private-registry/)
entry. Everything in `imagePipeline.registries` is merged into the
builder's `config.json` at Pod startup; no changes to your
`devcontainer.json` are needed.

End users do not provision any of this — the banner shows a generic
"build failed" state if credentials are missing, and the operator reads
the Job logs to diagnose.

## Generating `devcontainer-lock.json`

Djinn requires a committed lockfile so rebuilds do not drift when a
Feature author cuts a new patch. Generate it locally using the
reference CLI:

```bash
npm install -g @devcontainers/cli
devcontainer features info lock --workspace-folder .
git add .devcontainer/devcontainer-lock.json
git commit -m "chore: lock devcontainer features"
git push
```

The command reads `.devcontainer/devcontainer.json`, resolves every
Feature reference to an OCI digest, and writes `devcontainer-lock.json`
next to it.

Refresh the lock whenever you intentionally bump a Feature version:

```bash
devcontainer features info lock --workspace-folder . --force
```

## Troubleshooting

### "Missing devcontainer-lock.json" banner never clears

Re-run the lock command and make sure the file is actually committed
(`git log -- .devcontainer/devcontainer-lock.json`). The image controller
reads from the bare mirror's HEAD, so uncommitted local changes do not
count.

### "Image build failed" banner

Click **Rebuild** in the banner — this nulls the cached content hash so
the next mirror-fetch tick re-enqueues a fresh Job even if nothing in
the repo changed. If the build still fails, ask an operator to pull the
Job logs:

```bash
kubectl logs -n djinn -l djinn.app/project-id=<project-uuid>,djinn.app/build=true --tail=-1
```

Common causes:

- Private base image with no pull credential — see
  "Private registry authentication" above.
- Feature using an Alpine base — switch to a glibc base.
- BuildKit OOM — bump `imagePipeline.buildkitd.resources.limits.memory`
  in the Helm values.

### Banner says "Set up your devcontainer" but I already committed one

Djinn detects devcontainers on each mirror fetch, which runs on a 60s
cadence by default. If the banner is still stale, trigger a manual
refresh by re-visiting the project page or nudging the mirror
(`project_add_from_github` is idempotent and triggers a fresh fetch).

### Build works locally with `devcontainer build` but fails in Djinn

Likely a Feature that assumes network access to something outside the
cluster (for example, `apt` repos behind a corporate proxy). Djinn's
BuildKit daemon runs with standard cluster egress — if your node pool
is network-restricted, add your proxy's CA cert and HTTP(S) proxy env
to the build by overriding `containerEnv` in `devcontainer.json`:

```json
{
  "containerEnv": {
    "HTTP_PROXY": "http://proxy.internal:3128",
    "HTTPS_PROXY": "http://proxy.internal:3128",
    "NO_PROXY": ".svc.cluster.local,127.0.0.1,localhost"
  }
}
```

### Feature conflict errors

If two Features both try to install the same runtime (for example, two
different Node versions), the build fails at the `installsAfter`
ordering step. Pick one Feature and remove the other from
`devcontainer.json` — the lockfile regen will clean up.

### I need a base image with CUDA / GPU drivers

That is on the roadmap but not Phase 3. Today, pin a CUDA devcontainer
base (for example, `nvidia/cuda:12-devel-ubuntu22.04` or the
`devcontainers/images` CUDA variants) and add the Djinn Feature on top.
GPU scheduling at the Pod level is a separate operator-side change.

## Reference

- Spec: <https://containers.dev/implementors/spec/>
- Features registry: <https://containers.dev/features>
- CLI reference: <https://github.com/devcontainers/cli>
- Djinn agent-worker Feature: <https://github.com/djinnos/djinn-agent-worker-feature>
