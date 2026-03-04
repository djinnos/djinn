---
tags:
    - adr
    - agent
    - goose
    - phase-5
title: 'ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning'
type: adr
---
# ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning

## Status: Accepted

## Context

The original plan called for agent dispatch via the `summon` crate — a uniform subprocess interface for spawning Claude Code, OpenCode, Codex, and other CLI-based coding agents. Each agent session was a child process monitored by a tokio task within the AgentSupervisor actor. This approach provided:

- Multi-agent support (any CLI that speaks MCP)
- Process-level crash isolation
- Simple monitoring (PID tracking, SIGTERM/SIGKILL)

However, it had significant drawbacks:

- **No control over token usage** — subprocess agents manage their own context, the server can't observe or limit token consumption
- **No context compaction** — each agent manages its own context window independently; no fleet-level compaction strategy
- **Heavy process overhead** — each agent is a full CLI process (Node.js runtime for Claude Code, etc.), limiting concurrent session count
- **IPC overhead** — tool calls from agent to Djinn require MCP-over-localhost round-trips
- **MCP-connect bridge complexity** — the bridge (MCP-03) existed solely to inject project/task context into subprocess agent sessions, adding a translation layer

Investigation of [Goose](https://github.com/block/goose) (Block's open-source AI agent framework) revealed a pure Rust core library (`goose` crate) that provides production-grade agent orchestration as an importable dependency.

## Decision

**Replace summon subprocess spawning with the Goose library as an in-process agent harness.**

### Integration Architecture

```
goose = { path/git = "...", default-features = false }
```

`default-features = false` excludes the `code-mode` feature, which pulls in V8/Deno for JavaScript execution. Without it, no V8, no Node.js, no heavyweight runtimes.

### What Goose Provides

- **Agent struct** with streaming `reply()` → `BoxStream<AgentEvent>`
- **Session management** (SQLite-backed, per-session extension scoping)
- **Token tracking** (tiktoken-based, per-message and accumulated)
- **Context compaction** (auto-summarize at configurable threshold, default 80%)
- **20+ model providers** (Anthropic, OpenAI, Bedrock, Vertex, Databricks, LiteLLM...)
- **MCP client** for connecting to external tool servers
- **CancellationToken** support throughout (aligns with Djinn's shutdown strategy)
- **Per-session prompt override** via `set_system_prompt_override()` and `extend_system_prompt()`
- **Per-session extension scoping** — different tool sets per agent type

### Agent Execution Model

Agents become in-process async tasks within the AgentSupervisor actor:

```
AgentSupervisor (actor, owns N running Goose agents)
├── tokio::spawn(agent_1.reply(...))  // dev worker
├── tokio::spawn(agent_2.reply(...))  // task reviewer
└── tokio::spawn(agent_N.reply(...))  // phase reviewer
```

Each task's `JoinHandle` + `CancellationToken` replaces PID tracking. Graceful shutdown: cancel token → agent stops → WIP commit via GitActor.

### Tool Access: Direct Function Calls

Djinn's own tools (task_transition, memory_write, etc.) are exposed to agents via a custom Goose extension that calls Djinn's service layer directly — no MCP-over-localhost, no bridge. External tools (filesystem, web, user-provided MCPs) still go through Goose's MCP client.

### Session Storage

Goose's `SessionManager::new(data_dir)` pointed at `~/.djinn/sessions/` to co-locate with Djinn's data. Separate SQLite DB file (Goose manages its own schema), but under Djinn's directory tree.

### Credential Management

Server owns a credential vault in the Djinn DB (settings table, encrypted at rest). Supports VPS, WSL, and standalone deployment where the desktop doesn't spawn the server. When creating a Goose provider, Djinn loads the key from its vault and injects it. No server restart needed to add keys.

For OAuth-capable Goose providers (for example ChatGPT Codex and GitHub Copilot), Djinn also exposes OAuth setup through MCP so the UI can start the provider's `configure_oauth()` flow directly. Provider catalog responses include OAuth capability metadata and connection state (credential and/or OAuth token), so desktop flows can render a unified "connected" status without shelling out to Goose CLI.

### Fleet-Level Model Health

Djinn's existing model health system (`n8e4` — circuit breakers, cooldowns, rerouting) wraps Goose's provider selection. Flow:

1. Coordinator asks model health: "which models are healthy + have capacity?"
2. Djinn picks model, loads API key from vault
3. Goose creates Provider, runs agent
4. Provider errors bubble back → Djinn health tracker

Goose handles per-request retry; Djinn handles fleet-level routing. Goose is unaware of cross-session health.

### Prompt System

Embedded Rust templates (`include_str!()`) for dev, task reviewer, and phase reviewer agents — same approach as the Go server's `//go:embed` templates. Rendered with task data, injected via `agent.set_system_prompt_override()`. No scaffold file deployment needed.

### Future Extensibility

Goose's per-session extension system supports custom agent types with custom MCP servers and prompts (e.g., a "marketing expert" agent with marketing-specific tools). This is configuration-driven — no code changes to add new agent types.

## Consequences

**Positive:**

- Full control over token usage, context compaction, and session lifecycle from the server
- Zero IPC overhead for Djinn tool calls (direct function calls)
- Lightweight concurrent sessions (async tasks, not processes) — can run dozens
- 20+ model providers for free (no custom provider implementations)
- Per-session prompt and tool customization
- Eliminates MCP-connect bridge complexity (MCP-03 dropped)
- Eliminates scaffold deployment system (AGENT-12 dropped)
- Credential vault supports all deployment modes (desktop, VPS, WSL)

**Negative:**

- Tight coupling to Goose library — if Goose changes its API, Djinn must adapt
- Loss of subprocess crash isolation (mitigated by `catch_unwind` + tokio task boundaries)
- Goose's session DB is separate from Djinn's DB (two SQLite files to manage)
- Cannot support non-Goose agent CLIs (Claude Code, OpenCode, Codex dropped as direct options)
- Goose is a large dependency (though much smaller without V8/code-mode)

## Alternatives Considered

1. **Keep summon + subprocess model** — Proven approach, but lacks token/context control and adds IPC overhead. Rejected.
2. **Build custom agent harness from scratch** — Maximum control but massive effort to reimplement what Goose provides (providers, token counting, compaction, MCP client). Rejected.
3. **Use Rig (rig-rs) framework** — Less mature than Goose, smaller provider coverage, no session management. Rejected.
4. **Hybrid: Goose for core + summon for external agents** — Adds complexity for questionable benefit. Can revisit if multi-harness support is needed in v2. Rejected for v1.

## Relations

- [[Roadmap]] — Phase 5 (Agent Orchestration) and Phase 6 (Review)
- [[V1 Requirements]] — AGENT-01 through AGENT-12, MCP-03
- [[Language Selection — Compiler as AI Code Reviewer]] — ADR-001, Rust ecosystem choice enables Goose integration
- [[Server Lifecycle — Desktop-Managed Daemon with Graceful Restart]] — ADR-005, credential vault supplements desktop-managed lifecycle for VPS/WSL
- [[Agent Harness Scope]] — scope note for this discussion
