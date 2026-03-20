---
tags:
    - scope
    - agent
    - goose
    - reference
title: Agent Harness Scope
type: reference
---
# Agent Harness Scope

## In Scope

- Goose `goose` crate as Rust library dependency with `default-features = false` (no V8/Deno)
- AgentSupervisor actor wrapping Goose agents as tokio tasks (not subprocesses)
- Direct function calls from agents to Djinn service layer via custom Goose extension
- Goose SessionManager redirected to `~/.djinn/sessions/` for session/conversation persistence
- Credential vault in Djinn DB (settings table, encrypted at rest) for API keys
- Fleet-level model health (`n8e4`) wrapping Goose provider selection
- Goose owns provider implementations and per-request retry
- Embedded prompt templates (`include_str!()`) for dev, task reviewer, phase reviewer agents
- Per-session prompt and extension configuration via Goose's API
- MCP client via Goose for external tools (filesystem, web, user-provided MCP servers)
- CancellationToken-based graceful shutdown propagation to Goose agents

## Out of Scope

- **summon crate** — Dropped entirely. No subprocess-based agent spawning.
- **MCP-connect bridge (MCP-03)** — Dropped. Direct function calls replace it.
- **Scaffold file deployment (AGENT-12)** — Dropped. Goose prompt API + embedded templates replace it.
- **Support for non-Goose agent CLIs** (Claude Code, OpenCode, Codex) — Dropped for v1. Goose is the sole harness.
- **Custom agent types** (marketing expert, etc.) — v2 feature. Architecture supports it via per-session extension scoping, but not implemented in v1.
- **Goose code-mode / V8 integration** — Explicitly excluded. No JavaScript execution in agent sessions.
- **Replacing Goose's session SQLite** with Djinn's DB — v2 consideration. v1 uses Goose's session DB as-is at redirected path.

## Preferences

- Prefer Goose's built-in provider implementations over custom ones
- Prefer direct function calls over MCP for Djinn-internal tool access
- Keep Goose as a thin execution layer — Djinn owns dispatch decisions, routing, health
- Prompt templates should follow the Go server's structure (dev.md, task-reviewer.md, phase-reviewer-batch.md)
- Credential vault should support runtime key addition without server restart

## Requirement Changes

| Requirement | Change | Reason |
|---|---|---|
| AGENT-03 | Rewrite: "Agent dispatch via Goose library (in-process async tasks)" | Replaces summon subprocess model |
| AGENT-12 | Drop | Scaffold system replaced by Goose prompt API |
| MCP-03 | Drop | Bridge replaced by direct function calls |
| CFG-04 | Narrow: "Capacity limits and routing preferences only" | Credentials managed by vault + Goose providers |
| AGENT-01 | Update: Sessions are tokio tasks, not subprocesses | In-process model |
| AGENT-09 | Update: CancellationToken → agent stop (not SIGTERM → kill) | In-process shutdown |

## New Requirements

| ID | Requirement | Reason |
|---|---|---|
| AGENT-16 | Credential vault in Djinn DB with encrypted API key storage | VPS/WSL/standalone deployment support |
| AGENT-17 | Goose provider creation from vault credentials at dispatch time | Runtime key management |
| AGENT-18 | Per-session Goose Agent configuration (prompt override, extension scoping) | Agent type differentiation |

## Task Board Changes

| Task | Action | Reason |
|---|---|---|
| d9s4 (AgentSupervisor) | Update title/description | No longer summon-based; wraps Goose tokio tasks |
| 1tst (MCP-connect bridge) | Close/drop | MCP-03 eliminated |
| 1nby (Scaffold system) | Close/drop | AGENT-12 eliminated |
| lm7a (Review agents) | Update description | Uses Goose, not summon |
| qhb4 (Server lifecycle) | Update description | Add credential vault scope, VPS/standalone scenarios |

## Relations

- [[Roadmap]] — Phase 5, Phase 6
- [[V1 Requirements]] — AGENT-*, MCP-03, CFG-04
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]]
- [[Server Lifecycle — Desktop-Managed Daemon with Graceful Restart]] — ADR-005, credential management supplements lifecycle