# Deploying Djinn

Djinn is a Kubernetes-native control plane. The server doesn't just *run* on
Kubernetes — it *uses* the Kubernetes API at runtime to dispatch every agent
task-run, graph-warm, and verification as a short-lived Job/Pod. So a cluster
is a hard requirement, but it can be **any conformant Kubernetes**: a single
cheap VPS running [k3s](https://k3s.io), local [kind](https://kind.sigs.k8s.io),
or a managed cluster (EKS / GKE / AKS) or self-managed kubeadm.

There is **one Helm chart** (`deploy/helm/djinn`) for every environment. The
only thing that changes between a laptop and production is whether the *backing
services* (Postgres, Qdrant, the image registry) are bundled in-cluster or
pointed at managed equivalents. Everything is just different `values`.

## Choose your path

| You have | Guide | What you get |
|----------|-------|--------------|
| A laptop, want to hack on Djinn | [Local stack (Tilt + kind)](../DEVELOPMENT.md#local-stack-tilt--kind) | Full stack in kind via `tilt up`, built from source |
| A VPS (or want to rent one) | **[Single VPS with k3s](vps.md)** | Everything bundled on one box: Postgres, Qdrant, in-cluster registry, TLS via Let's Encrypt |
| A managed / production cluster | **[Managed Kubernetes (EKS / GKE / AKS)](kubernetes.md)** | External Postgres (RDS / Cloud SQL), managed registry (ECR / GAR / ACR), cloud identity, GitOps |
| An AI agent with a shell | **[AI-assisted install](AGENT.md)** | Paste one prompt; the agent interviews you and runs the deploy |

**AI-assisted install** — paste this into Claude Code, Cursor, or any agent
with shell access to your target machine:

```
Fetch https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/AGENT.md and follow it to deploy Djinn for me.
```

## What you need

Four things, regardless of environment:

1. **A Kubernetes cluster** — one k3s node is enough. Nodes that run the image
   pipeline need two sysctls (see [Node prerequisites](#node-prerequisites)).
2. **A public URL** — OAuth callbacks (GitHub login, MCP clients) need a stable
   `env.publicUrl`. In practice: a domain with an A record and TLS. Plain HTTP
   on `127.0.0.1` is fine for local dev only.
3. **A GitHub App** — powers login, repo clone/push, and PRs. Djinn can
   create it for you in one click from the sign-in screen (manifest flow), or
   follow the [manual guide](../GITHUB_APP_SETUP.md).
4. **At least one LLM provider** — bootstrap API keys via Helm values
   (`secrets.providers.*`), or skip that entirely and have each user connect
   their own under **Settings → Models** after login (API keys, or OAuth
   sign-in for ChatGPT/Codex and GitHub Copilot).

## What's bundled, what you can swap

Every backing service ships in the chart with a production-grade external
alternative one value away:

| Service | Bundled (default for VPS/local) | External (default for production) |
|---------|--------------------------------|-----------------------------------|
| **Postgres 16** — all state | `postgres.enabled: true` StatefulSet | RDS / Cloud SQL / any Postgres 16+: `postgres.enabled: false` + `database.existingSecret` (or `database.externalUrl`) |
| **Qdrant** — vectors | `qdrant.enabled: true` StatefulSet | External Qdrant: `qdrant.enabled: false` |
| **OCI registry** — per-project devcontainer images | In-cluster [Zot](https://zotregistry.dev): `imagePipeline.zot.enabled: true` | ECR / GAR / ACR: `imagePipeline.registryHost` + `buildkitd.credHelpers` |
| **TLS** | cert-manager + Let's Encrypt through the cluster ingress | Cloud LB certs (ACM on ALB, etc.) |
| **Tracing** (optional) | Self-hosted [Langfuse](https://langfuse.com) (`deploy/langfuse-local` for dev) | Langfuse Cloud via `langfuse.*` keys |

The full knob-by-knob reference lives in **[Configuration](configuration.md)**.

**Pre-task lifecycle hooks** — projects can declare shell commands (database
migrations, dependency installs, fixture seeds) that run in the task-run Pod
before the agent starts.  See
**[lifecycle-pre-task.md](lifecycle-pre-task.md)** for examples (Rails, Django,
Prisma, raw SQL), validation constraints, failure policies, and the
`task_run_pretask_ran` activity event contract.

## How installs and upgrades work

```bash
helm upgrade --install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace \
  -f my-values.yaml
```

That one command is both fresh install and upgrade, with bundled or external
Postgres, on any cluster.

**Migrations are automatic.** The server Deployment runs `djinn-server
--migrate-only` in an initContainer before the app container starts: it waits
for Postgres to be reachable, takes the sqlx migrator advisory lock, applies
pending migrations, and exits. With `replicas: 1` + `maxSurge: 1` exactly one
migrator runs at a time and the advisory lock serialises any overlap — rolling
upgrades need no manual step. (To run migrations by hand for debugging:
`djinn-server --migrate-only` against the DB.)

The chart is not published to a chart registry yet — install from a clone of
this repo, or point ArgoCD/Flux at the `deploy/helm/djinn` path.

## Images and versioning

Published images live at `ghcr.io/djinnos/*` and are **tagged without a
leading `v`** — `ghcr.io/djinnos/djinn-server:0.6.69`, not `:v0.6.69` (the `v*`
form is only the git tag). Four images make up a release:

| Image | Role | Pin via |
|-------|------|---------|
| `djinn-server` | Control plane | `image.server` |
| `djinn-agent-runtime` | Task-run pods | `image.runtime` |
| `djinn-buildkitd` | Image-build daemon | `imagePipeline.buildkitd.image` |
| `djinn-image-builder` | devcontainer build Jobs | `imagePipeline.builderImage` |

Pin `image.server` + `image.runtime` to the same release for anything that
isn't a throwaway demo, and bump them together.

## Node prerequisites

The image pipeline runs BuildKit **rootless** via user namespaces. Every node
that may schedule the `buildkitd` pod needs:

```sh
sysctl -w kernel.unprivileged_userns_clone=1   # absent on newer kernels — that's fine
sysctl -w user.max_user_namespaces=28633        # or higher
```

Persist via `/etc/sysctl.d/`. k3s nodes usually ship with both; kind inherits
host sysctls; some managed node images need them set via a startup script or a
privileged DaemonSet. Quick check:

```sh
kubectl debug node/<node> -it --image=busybox -- sh -c \
  'cat /proc/sys/user/max_user_namespaces'
```

## After the install

1. Open `https://<your-host>` and **sign in with GitHub** — the **first user to
   sign in becomes the admin**.
2. **GitHub App setup:** If self-setup is enabled (`env.enableSelfSetup: true`),
   the server prints a **one-time setup URL** in its boot log on first startup.
   Open that URL in your browser to create the App via GitHub's manifest flow,
   install it on your repos, and land on the task board — all in two clicks.
   For production deployments (self-setup disabled), provide credentials via
   `secrets.githubApp.*` in your Helm values or an `existingSecret` before
   deploying. ([Details](../GITHUB_APP_SETUP.md).)
3. Connect a model under **Settings → Models** (unless you bootstrapped keys
   via values), add a project from GitHub, and write your first proposal.
4. Point your editor at the MCP endpoint:
   `claude mcp add --transport http djinn https://<your-host>/mcp`
