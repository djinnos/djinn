---
tags:
    - research
    - goose
    - agent
    - phase-5
title: Goose Library Integration Research - Phase 5
type: research
---
# Goose Library Integration Research - Phase 5

## Summary

Explored the Goose codebase at `/home/fernando/git/references/goose` and the existing Djinn server to map integration points for the AgentSupervisor (d9s4). Goose provides a comprehensive agent framework as a Rust library.

## Key Goose APIs

### Agent Lifecycle
- `Agent::with_config(AgentConfig)` — create agent with custom config
- `agent.reply(message, session_config, cancel_token)` → `BoxStream<AgentEvent>`
- `AgentEvent`: Message, McpNotification, ModelChange, HistoryReplaced
- CancellationToken checked at key points in agent loop (lines 1126, 1179, 1343 in agent.rs)

### Session Management
- `SessionManager::new(data_dir)` — creates SQLite DB at `{data_dir}/sessions.db`
- `create_session(working_dir, name, SessionType)` → Session
- SessionType: User, Scheduled, SubAgent, Hidden, Terminal, Gateway

### Extension System (Tool Access)
- **Frontend extension** — pass `rmcp::Tool` instances directly. Best for Djinn tools: no MCP serialization, tools routed to custom handler.
- **Platform extension** — in-process with direct agent access. Alternative for deeper integration.
- Per-session scoping: `agent.add_extension(config, &session_id)` — different sessions get different tools

### Provider Creation
- `providers::create_with_named_model("anthropic", "claude-opus-4-6", extensions)`
- Credentials via `Config::global().set_secret("ANTHROPIC_API_KEY", key, true)`
- Supports 20+ providers: Anthropic, OpenAI, Bedrock, Vertex, Databricks, LiteLLM...

### Prompt System
- `PromptManager::set_system_prompt_override(template)` — replace entire system prompt
- `PromptManager::add_system_prompt_extra(key, instruction)` — merge additional directives
- SystemPromptBuilder with `.with_extensions()`, `.with_frontend_instructions()`, `.build()`

### Cargo Features
- `default = ["code-mode"]` which pulls in V8/Deno via `pctx_code_mode`
- `default-features = false` excludes V8 — keeps all agent/provider/extension functionality

## Existing Server Integration Points

### AgentSupervisor Stub
- `src/actors/supervisor.rs` — stub with `has_session()` and `dispatch()` returning defaults
- CoordinatorActor already calls these methods — d9s4 provides real implementation

### CoordinatorActor (1u1b — closed)
- 4-arm tokio::select!: cancel, messages, DjinnEvent broadcast, 30s tick
- Dispatch: filters ready tasks, calls `supervisor.has_session()` + `supervisor.dispatch()`
- Stuck detection: finds in_progress tasks with no active session

### GitActor
- `create_worktree(task_short_id, branch)` → PathBuf at `.djinn/worktrees/{short_id}/`
- `remove_worktree(path)` for cleanup
- All writes serialized through actor

### HealthTracker (n8e4 — closed)
- `is_available(model_id)`, `record_success/failure(model_id)`
- Circuit breaker: 3 consecutive failures → 5min cooldown, exponential backoff

### Settings/Credentials
- `SettingsRepository::get/set(key, value)` — key-value in DB
- Credential vault (AGENT-16) needs new migration for encrypted storage

## Recommendations

1. **Use Frontend extension type** for Djinn tools — rmcp::Tool instances already exist in codebase
2. **Goose Config::global().set_secret()** for credential injection — set before each provider creation
3. **include_str!() templates** following Go server pattern (dev.md, task-reviewer.md, phase-reviewer-batch.md)
4. **SessionManager at ~/.djinn/sessions/** — separate SQLite DB, co-located with Djinn data
5. **GooseSessionHandle = JoinHandle + CancellationToken + session_id** — replaces PID tracking

## Relations

- [[Roadmap]] — Phase 5 (Agent Orchestration)
- [[V1 Requirements]] — AGENT-01 through AGENT-11, AGENT-16/17/18
- [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning|ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]]
- [[Agent Harness Scope]] — scope boundaries