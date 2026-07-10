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
  <a href="https://youtu.be/cewtCRdkUuk"><strong>Demo video</strong></a> ·
  <a href="#how-it-works"><strong>How it works</strong></a> ·
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

1. **Propose** — anyone writes a spec: a problem, a goal, acceptance criteria. In the editor, or by letting Djinn draft it with you in chat.
2. **Review & sign off** — feedback lands revision by revision; sign-offs go stale if the spec changes after, so approval always means *this* version.
3. **Build** — graduation decomposes the proposal into epics and tasks, dispatched by priority and dependency order, each in its own Kubernetes Job. Freeze or abort mid-build to rework the spec and go again.
4. **AI review** — a reviewer agent checks the work against the acceptance criteria; rejections loop back.
5. **Pull request** — approved work opens a PR via your GitHub App. You review, you merge.

Prefer to skip the ceremony? Tasks and epics can also be created directly on the board — the proposal layer is governance you opt into, not a gate you can't route around.

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

Each task routes to an **agent role** by type — Architect (read-only spikes), Planner, Developer, Reviewer, Lead — and Djinn runs its own LLM agent loop (no external runtime), resolving the model per task with precedence **user → project → global** from a live [models.dev](https://models.dev) catalog. The only admission control is each user's per-model concurrency cap.

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

Just want a quick local test (or to hack on Djinn itself)? `tilt up` brings
the whole stack up in a local kind cluster, built from source — see
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

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

Rust workspace in `server/`, React UI in `ui/`, embedded into the server binary. `tilt up` is the whole dev loop; the local stack, workspace layout, test setup, and CI notes are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Community

- [GitHub Issues](https://github.com/djinnos/djinn/issues) — Bug reports and feature requests
- [GitHub Discussions](https://github.com/djinnos/djinn/discussions) — Ideas and general conversation

## License

[Business Source License 1.1](LICENSE). Free to self-host and use in production: the only restriction is offering Djinn itself as a competing hosted service. Each version converts to Apache 2.0 four years after release. © 2026 Djinn AI, Inc.
