<p align="center">
  <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/icon.png?raw=true" width="128" height="128" alt="Djinn" />
</p>

<h1 align="center">Djinn</h1>

<p align="center">
  <strong>Manage AI agents, not terminals.</strong>
  <br />
  A Kubernetes-native control plane for AI coding agents. Multi-user, multi-project, multi-model — you review every change before it merges.
</p>

<p align="center">
  <a href="#quick-start-local"><strong>Quick start</strong></a> ·
  <a href="#architecture"><strong>Architecture</strong></a> ·
  <a href="#deploy-kubernetes"><strong>Deploy</strong></a> ·
  <a href="https://djinnai.io"><strong>Website</strong></a>
</p>

<br />

Djinn turns a kanban board into an agent orchestrator. Organize work across any number of repositories as epics and tasks; Djinn dispatches each task to an AI agent running in its own isolated **Kubernetes Job**, reviews the result against your acceptance criteria, and opens a pull request for you to merge.

Instead of juggling terminal windows and switching between models and repos, you direct work from a board. The `djinn-server` control plane is the single source of truth — the web UI, Claude Code, Cursor, and any MCP client are all just consumers of the same API.

<br />

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/kanban.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/kanban.jpg?raw=true" width="800" alt="Djinn — Kanban board with parallel AI agents across multiple projects" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/epics.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/epics.jpg?raw=true" width="800" alt="Djinn Roadmap — Epic dependency graph with tasks and blockers" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/memory.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/memory.jpg?raw=true" width="800" alt="Djinn Memory Graph — Knowledge base visualization with connected notes" />
  </a>
</p>

## How it works

```
  Create tasks ──▶ Run ──▶ Agents work in parallel ──▶ AI review ──▶ Pull request
       │            │              │                       │              │
   Kanban /     Coordinator   One Kubernetes Job       Reviewer agent   You merge
   epics /      dispatches    per task-run, each       checks your
   MCP          by priority   in its own isolated      acceptance
                & dependency   workspace               criteria
```

1. **Create tasks** — Features, bugs, tech debt. Organize them as epics with dependencies and blockers across any number of projects.
2. **Run** — The coordinator dispatches ready tasks by priority and dependency order, gated by each user's per-model concurrency limit.
3. **Agents work in parallel** — Each task-run executes in its own Kubernetes Job, in a per-project devcontainer image, with an isolated git workspace.
4. **AI review** — A reviewer agent checks the work against your acceptance criteria; rejected work loops back for another pass.
5. **Pull request** — Approved work is pushed and a PR is opened via your GitHub App (or squash-merged directly when no App is configured). Nothing ships without your approval.

## Architecture

Djinn is a Rust control plane (`djinn-server`) that acts as a Kubernetes controller. It dispatches each task-run as a short-lived Job whose pod runs the agent in-cluster and connects back to the server over RPC. The web client is a React SPA embedded into the server binary, so a deployed server serves the UI same-origin behind any ingress.

```
            Web UI · Claude Code · Cursor · any MCP client
                              │  (OAuth 2.1)
                              ▼
        ┌──────────────────────────────────────────────────┐
        │            djinn-server  (control plane)          │
        │  HTTP API · embedded React UI · MCP (/mcp)        │
        │  coordinator · per-user elastic slot pool         │
        │  mirror fetcher · image controller · RPC listener │
        └───────┬─────────────────────┬────────────────────┘
       dispatch │                     │ dispatch
                ▼                     ▼
   ┌─────────────────────┐   ┌──────────────────────┐
   │  task-run K8s Job    │   │   image build Job    │
   │  djinn-agent-worker  │   │   BuildKit ─▶ OCI    │
   │  in per-project      │   │   registry (Zot /    │
   │  devcontainer image  │   │   ECR / GCR / ACR)   │
   └──────────┬───────────┘   └──────────────────────┘
              │ RPC (:8443)
   ┌──────────┴───────────────────────────────────────┐
   │   Postgres 16 (state, JSONB)   ·   Qdrant (vectors)│
   └───────────────────────────────────────────────────┘
```

**Components**

| Component | Role |
|-----------|------|
| `djinn-server` | Control plane: HTTP API, embedded UI, MCP endpoint, OAuth server, task coordinator, slot pool, repo mirror fetcher, per-project image controller, worker RPC listener. |
| `djinn-agent-worker` | Runs inside each task-run pod. Drives the agent role sequence stage-by-stage against an isolated `/workspace`, streaming results back over RPC. |
| Postgres 16 | All state — tasks, epics, sessions, notes, users, encrypted credentials, code-graph cache (JSONB throughout). |
| Qdrant | Vector store for code-chunk and note embeddings (semantic + hybrid search). |
| BuildKit + registry | Builds a per-project devcontainer image (from detected stack) that every task-run for that project runs in. |

**Agent roles** — Each task is routed to a role based on its type:

- **Architect** — read-only consultant for spikes; deep structural reasoning, no board changes.
- **Planner** — owns planning, decomposition, and review-grooming tasks.
- **Developer** — does the code change, commits, and pushes to the project mirror.
- **Reviewer** — judges the work against acceptance criteria; approves or sends it back.
- **Lead** — handles escalations and interventions.

**Model routing** — Djinn runs its own LLM agent loop (no external runtime). Models are resolved per task with precedence **user → project → global**, drawn from a live [models.dev](https://models.dev) catalog. The slot pool is elastic; the sole admission control is each user's per-model concurrency cap.

## Quick start (local)

The full stack runs in a local [kind](https://kind.sigs.k8s.io) cluster, orchestrated by [Tilt](https://tilt.dev). One command brings up the cluster, registry, server, Postgres, Qdrant, the image pipeline, and a self-hosted Langfuse for tracing.

**Prerequisites:** Docker, [kind](https://kind.sigs.k8s.io), `kubectl`, [Helm](https://helm.sh), [Tilt](https://tilt.dev), [pnpm](https://pnpm.io) (Node), and `openssl`.

The image pipeline runs BuildKit rootless via user namespaces. On the host (kind inherits host sysctls):

```sh
sudo sysctl -w kernel.unprivileged_userns_clone=1
sudo sysctl -w user.max_user_namespaces=28633   # or higher
```

Then, from the repo root:

```bash
tilt up
```

Tilt bootstraps the kind cluster (`djinn`) + a local registry, builds `djinn-server` and `djinn-agent-worker`, embeds the freshly built UI, installs the Helm chart, and port-forwards:

| Port | Service |
|------|---------|
| `:3000` | djinn API + web UI |
| `:8443` | worker RPC |
| `:5432` | Postgres |
| `:6333` / `:6334` | Qdrant (HTTP / gRPC) |
| `:5000` | Langfuse dashboard |
| `:9091` | MinIO console |

Open the UI at **http://127.0.0.1:3000**. `tilt down` removes the Helm release but leaves the cluster up; `kind delete cluster --name djinn` tears it down completely.

> The heavy build steps (`djinn-binaries`, `djinn-ui-dist`, runtime base image) are **manual** triggers in the Tilt UI — hit refresh on `djinn-binaries` to recompile after Rust changes; the server image and pod roll follow automatically.

## Deploy (Kubernetes)

djinn is Kubernetes-native — it dispatches every agent run as a Job/Pod — so it
needs a cluster, but it can be **any** conformant Kubernetes. One Helm chart
(`deploy/helm/djinn`) covers every environment; the only thing that changes is
whether the backing services (Postgres, Qdrant, registry) are bundled or
managed:

- **Local** — `tilt up` brings up the whole stack in kind ([Quick start](#quick-start-local)).
- **Single node / self-hosted / VPS** — everything bundled on a one-box k3s cluster.
- **Any managed or self-managed cluster (EKS / GKE / AKS / kubeadm)** — external Postgres/registry, cloud identity, GitOps.

```bash
helm upgrade --install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace \
  -f my-values.yaml
```

That one command handles fresh installs and upgrades, bundled or external
Postgres — migrations run automatically in the server's migrate initContainer.

**→ Full guide, per-environment values, and the production/EKS overlay: [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).**

## Features

### ⚡ Parallel execution

Each task-run is an isolated Kubernetes Job in its own workspace. Run many agents at once across many repos; scale by adding nodes, not terminal tabs.

### 📁 Multi-project

Microservices, monorepos, many repositories — Djinn manages them all. Each project gets its own devcontainer image (built from auto-detected stack), task board, code graph, and knowledge base.

### 🔀 Mix & match models

Djinn owns its own agent loop and talks to providers directly: Anthropic, OpenAI, Google, Fireworks, ChatGPT/Codex and GitHub Copilot (OAuth), Vertex AI, Bedrock, Azure OpenAI, plus any OpenAI-compatible endpoint from the models.dev catalog. Use one model for coding, another for reviews, another for research — configured per user, per project, and per role.

### 🧠 Persistent memory

Decisions, patterns, and architectural rules live in a searchable, DB-backed knowledge base of linked notes (`[[wikilinks]]`). ADRs are first-class: agents propose them, you accept or reject. Hybrid lexical + semantic search keeps the right context in front of every agent.

### 🔍 Code intelligence

A per-project **code graph** built from SCIP indexers across 8 languages powers impact analysis, dependency cycles, coupling hubs, dead-symbol detection, and complexity metrics (cognitive + cyclomatic). Agents query it over MCP; you explore it visually in the UI.

### 👥 Multi-user

Self-hosted for your whole team. Each user logs in via GitHub, brings their own provider credentials (encrypted at rest, with org-shared fallback), and has private chat — over a shared board. Admins can manage users and act on their behalf.

### ✅ Built-in review

AI reviewers check each task against your acceptance criteria. You review the finished work and decide when to merge.

## Connect your tools

The server exposes its full surface — tasks, epics, memory, code graph, projects, providers, settings, execution — over **MCP at `POST /mcp`**, served over streamable HTTP and gated by Djinn's own OAuth 2.1 (it federates GitHub for login and issues opaque, audience-bound tokens). Any MCP client connects by pointing at the endpoint and completing the OAuth flow; clients discover the authorization server via the standard `/.well-known/` metadata.

**Claude Code**

```bash
claude mcp add --transport http djinn https://<your-host>/mcp
```

**Cursor** — add to `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "djinn": { "url": "https://<your-host>/mcp" }
  }
}
```

**Local dev** uses the same shape against the Tilt server — see [`.mcp.json`](.mcp.json):

```json
{
  "mcpServers": {
    "djinn": { "type": "http", "url": "http://127.0.0.1:3000/mcp" }
  }
}
```

### What's available over MCP

| Tool group | Examples |
|-----------|----------|
| **Tasks** | `task_create`, `task_list`, `task_show`, `task_transition`, `task_claim`, `board_health` |
| **Epics** | `epic_create`, `epic_list`, `epic_tasks`, `epic_close` |
| **Memory / notes** | `memory_write`, `memory_read`, `memory_search`, `memory_graph`, `memory_build_context` |
| **Code graph** | `code_graph_search`, `code_graph_impact`, `code_graph_complexity`, `code_graph_coupling`, `code_graph_cycles` |
| **Proposals (ADRs)** | `propose_adr_list`, `propose_adr_accept`, `propose_adr_reject` |
| **Projects** | `project_list`, `project_add_from_github`, `project_environment_config_get`, `get_project_stack` |
| **Providers** | `provider_catalog`, `provider_models`, `credential_set`, `model_health` |
| **Settings** | `settings_get`, `settings_set`, `user_settings_get`, `user_settings_set` |
| **Execution** | `execution_kill_task`, `session_active`, `session_messages`, `task_timeline` |

## Configuration

- **[GitHub App setup](docs/GITHUB_APP_SETUP.md)** — connect the server to a GitHub App so repo operations (clone, push, PRs) run under installation tokens and commits are attributed to `djinn-bot[bot]`. Login is federated through the same App.
- **Providers** — bootstrap API keys via `secrets.providers.*`, or have each user connect their own under Settings → Models (including OAuth providers like ChatGPT/Codex and Copilot).
- **Credential vault** — provider credentials are encrypted with AES-256-GCM using the key in `secrets.vaultKey.key`. Keep it stable across upgrades or existing credentials become undecryptable.

## Development

The Rust workspace lives in `server/` (binary `djinn-server` plus ~16 crates under `server/crates/`); the web client is in `ui/` (React + Vite + TypeScript, pnpm). The UI is compiled into the server binary via `rust-embed`, and the TypeScript MCP types are generated from the server's live tool schemas (`pnpm --dir ui mcp:types`).

Tests run against a dedicated throwaway Postgres (not the dev cluster), started via Docker Compose on `:5433`:

```bash
docker compose up -d postgres-test   # test-only Postgres
make test                            # djinn-db tests
make test-all                        # whole workspace (cargo nextest)
make sqlx-check                      # fail if the offline sqlx cache is stale
```

The workspace `.cargo/config.toml` defaults `DATABASE_URL` to the `:5433` instance, so plain `cargo test`/`cargo build` just work. See the `Makefile` for the full target list.

## Community

- [GitHub Issues](https://github.com/djinnos/djinn/issues) — Bug reports and feature requests
- [GitHub Discussions](https://github.com/djinnos/djinn/discussions) — Ideas and general conversation

## License

Proprietary. © 2026 Djinn AI, Inc. Free to use during beta.
