<p align="center">
  <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/icon.png?raw=true" width="128" height="128" alt="Djinn" />
</p>

<h1 align="center">Djinn</h1>

<p align="center">
  <strong>From proposal to pull request.</strong>
  <br />
  A Kubernetes-native platform where your team proposes and approves work, and AI agents build it.
  On your cluster, with your models, behind your review.
</p>

<p align="center">
  <a href="https://youtu.be/f-S3ju-GjCs"><strong>Demo video</strong></a> ·
  <a href="#quick-start-local"><strong>Quick start</strong></a> ·
  <a href="#architecture"><strong>Architecture</strong></a> ·
  <a href="#deploy"><strong>Deploy</strong></a> ·
  <a href="https://djinnai.io"><strong>Website</strong></a>
</p>

<br />

Djinn turns ideas into reviewed, merged code through one governed pipeline. Anyone on the team writes a **proposal**: a living spec, not a chat prompt. Product and engineering discuss it, leave feedback, and sign off. When it graduates, Djinn plans the work, decomposes it into epics and tasks across any number of repositories, and dispatches AI agents to build it, each in its own isolated **Kubernetes Job**. An AI reviewer checks every result against the acceptance criteria before a pull request is opened. **Nothing ships without your approval.**

The `djinn-server` control plane is the single source of truth: the web UI, Claude Code, Cursor, and any MCP client are all consumers of the same API. And because it all runs on your cluster with your provider credentials, your code and your spend never leave your control.

<br />

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/kanban.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/kanban.jpg?raw=true" width="800" alt="Djinn — Kanban board with parallel AI agents across multiple projects" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/proposals.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/proposals.jpg?raw=true" width="800" alt="Djinn Proposals — living specs moving from draft through review and sign-off to build" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/roadmap.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/roadmap.jpg?raw=true" width="800" alt="Djinn Roadmap — Epic dependency graph with tasks and blockers" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/code-graph.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/code-graph.jpg?raw=true" width="800" alt="Djinn Code Graph — per-project symbol graph powering impact analysis and code intelligence" />
  </a>
</p>

## How it works

```
  Propose ──▶ Review & sign off ──▶ Build ──▶ AI review ──▶ Pull request
     │               │                │            │              │
  Anyone        Product +      Epics & tasks   Reviewer       You merge
  writes a      engineering    dispatched as   agent checks
  spec; AI      discuss,       isolated K8s    acceptance
  helps refine  iterate,       Jobs, in        criteria;
  it            sign off       parallel        rejects loop back
```

1. **Propose** — Anyone writes a proposal: a problem, a goal, acceptance criteria. Proposals are global, collaborative specs that can target any number of projects. Use the editor, or open a proposal-scoped chat and let Djinn draft and refine the spec with you.
2. **Review & sign off** — The team leaves feedback on the spec; the author (or Djinn) addresses it revision by revision. Reviewers sign off, and sign-offs go stale if the spec changes after, so approval always means *this* version.
3. **Build** — Graduating a proposal turns it into epics and tasks. The coordinator dispatches ready tasks by priority and dependency order, gated by each user's per-model concurrency limit. Each task-run executes in its own Kubernetes Job, in a per-project devcontainer image, with an isolated git workspace. Changed your mind mid-build? Freeze or abort, edit the spec, re-sign, re-graduate.
4. **AI review** — A reviewer agent checks the work against the proposal's acceptance criteria; rejected work loops back for another pass.
5. **Pull request** — Approved work is pushed and a PR is opened via your GitHub App. You review, you merge.

Prefer to skip the ceremony? Tasks and epics can also be created directly on the board: the proposal layer is governance you opt into, not a gate you can't route around.

## Architecture

Djinn is a Rust control plane (`djinn-server`) that acts as a Kubernetes controller. It dispatches each task-run as a short-lived Job whose pod runs the agent in-cluster and connects back to the server over RPC. The web client is a React SPA embedded into the server binary, so a deployed server serves the UI same-origin behind any ingress.

```
            Web UI · Claude Code · Cursor · any MCP client
                              │  (OAuth 2.1)
                              ▼
        ┌──────────────────────────────────────────────────┐
        │            djinn-server  (control plane)          │
        │  HTTP API · embedded React UI · MCP (/mcp)        │
        │  proposals · coordinator · per-user slot pool     │
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
| `djinn-server` | Control plane: HTTP API, embedded UI, MCP endpoint, OAuth server, proposal pipeline, task coordinator, slot pool, repo mirror fetcher, per-project image controller, worker RPC listener. |
| `djinn-agent-worker` | Runs inside each task-run pod. Drives the agent role sequence stage-by-stage against an isolated `/workspace`, streaming results back over RPC. |
| Postgres 16 | All state — proposals, tasks, epics, sessions, notes, users, encrypted credentials, code-graph cache (JSONB throughout). |
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

## Deploy

djinn is Kubernetes-native — it dispatches every agent run as a Job/Pod — so it
needs a cluster. But **"a cluster" can be one cheap VPS**: [k3s](https://k3s.io)
is full Kubernetes in a single binary, and djinn's Helm chart bundles everything
else (Postgres, Qdrant, an in-cluster registry) on that one box.

```bash
# on a fresh Ubuntu/Debian VPS — k3s is Kubernetes in one binary
curl -sfL https://get.k3s.io | sh -
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml   # then install Helm

helm upgrade --install djinn deploy/helm/djinn \
  --namespace djinn --create-namespace \
  -f my-values.yaml    # bundled Postgres/Qdrant/registry, ingress host, secrets
```

One chart covers every environment — the only thing that changes is whether the
backing services are bundled or managed. Pick your path:

| Environment | Guide |
|-------------|-------|
| Single VPS — everything bundled, TLS via Let's Encrypt | [docs/deploy/vps.md](docs/deploy/vps.md) |
| Managed cluster (EKS / GKE / AKS) — RDS/Cloud SQL, ECR/GAR/ACR, cloud identity, GitOps | [docs/deploy/kubernetes.md](docs/deploy/kubernetes.md) |
| Every knob — external Postgres, registries, secrets, storage, scheduling | [docs/deploy/configuration.md](docs/deploy/configuration.md) |
| Overview — requirements, what's bundled vs swappable, how upgrades work | [docs/deploy](docs/deploy/README.md) |

**Or let your AI do it.** Paste this into Claude Code, Cursor, or any agent
with shell access to your target machine:

```
Fetch https://raw.githubusercontent.com/djinnos/djinn/main/docs/deploy/AGENT.md and follow it to deploy Djinn for me.
```

The agent interviews you (VPS or existing cluster, domain, keys), runs the
install phase by phase with verification checkpoints, and hands you a working
URL. `helm upgrade --install` handles fresh installs and upgrades alike —
migrations run automatically in the server's migrate initContainer.

## Beyond the pipeline

- **Multi-project** — microservices, monorepos, many repositories. Each project gets its own devcontainer image (auto-detected stack), task board, code graph, and knowledge base; one proposal can drive changes across several.
- **Your models, mixed & matched** — Anthropic, OpenAI, Google, Vertex, Bedrock, Azure, Fireworks, ChatGPT/Codex and Copilot via OAuth, plus any OpenAI-compatible endpoint from the models.dev catalog. One model for coding, another for review — per user, per project, per role.
- **Persistent memory** — a searchable, DB-backed knowledge base of linked notes. Agents extract what they learn from each task and carry it into the next.
- **Code intelligence** — a per-project code graph (SCIP, 8 languages): impact analysis, dependency cycles, coupling hubs, dead symbols, complexity. Queryable over MCP, explorable in the UI.
- **Multi-user** — GitHub login, per-user encrypted provider credentials with org-shared fallback, private chat over a shared board, per-user concurrency caps, admin role.

## Connect your tools

The server exposes its full surface — proposals, tasks, epics, memory, code graph, projects, providers, settings, execution — over **MCP at `POST /mcp`**, served over streamable HTTP and gated by Djinn's own OAuth 2.1 (it federates GitHub for login and issues opaque, audience-bound tokens). Any MCP client connects by pointing at the endpoint and completing the OAuth flow; clients discover the authorization server via the standard `/.well-known/` metadata.

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
| **Proposals** | `proposal_create`, `proposal_show`, `proposal_feedback_add`, `proposal_signoff`, `proposal_graduate`, `proposal_stop_build` |
| **Tasks** | `task_create`, `task_list`, `task_show`, `task_transition`, `task_claim`, `board_health` |
| **Epics** | `epic_create`, `epic_list`, `epic_tasks`, `epic_close` |
| **Memory / notes** | `memory_write`, `memory_read`, `memory_search`, `memory_graph`, `memory_build_context` |
| **Code graph** | `code_graph_search`, `code_graph_impact`, `code_graph_complexity`, `code_graph_coupling`, `code_graph_cycles` |
| **Projects** | `project_list`, `project_add_from_github`, `project_environment_config_get`, `get_project_stack` |
| **Providers** | `provider_catalog`, `provider_models`, `credential_set`, `model_health` |
| **Settings** | `settings_get`, `settings_set`, `user_settings_get`, `user_settings_set` |
| **Execution** | `execution_kill_task`, `session_active`, `session_messages`, `task_timeline` |

## Configuration

- **[Configuration reference](docs/deploy/configuration.md)** — every chart knob: external Postgres (RDS/Cloud SQL), managed registries, secrets delivery, storage, task-run scheduling, Langfuse.
- **[GitHub App setup](docs/GITHUB_APP_SETUP.md)** — connect the server to a GitHub App so repo operations (clone, push, PRs) run under installation tokens and commits are attributed correctly. Login is federated through the same App — or use the one-click manifest flow on the sign-in screen.
- **Providers** — bootstrap API keys via `secrets.providers.*`, or have each user connect their own under Settings → Models (including OAuth providers like ChatGPT/Codex and Copilot).
- **Credential vault** — provider credentials are encrypted with AES-256-GCM using the key in `secrets.vaultKey.key`. Keep it stable across upgrades or existing credentials become undecryptable.

## Development

Rust workspace in `server/`, React UI in `ui/`, embedded into the server binary. `tilt up` is the whole dev loop ([Quick start](#quick-start-local)); workspace layout, test setup, and CI notes are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Community

- [GitHub Issues](https://github.com/djinnos/djinn/issues) — Bug reports and feature requests
- [GitHub Discussions](https://github.com/djinnos/djinn/discussions) — Ideas and general conversation

## License

[Business Source License 1.1](LICENSE). Free to self-host and use in production: the only restriction is offering Djinn itself as a competing hosted service. Each version converts to Apache 2.0 four years after release. © 2026 Djinn AI, Inc.
