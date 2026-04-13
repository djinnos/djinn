---
title: ADR-004: Execution Settings — Per-Role Model Priority and Per-Model Session Limits
type: adr
tags: ["adr","execution","settings","configuration"]
---

# ADR-004: Execution Settings — Per-Role Model Priority and Per-Model Session Limits

## Status: Accepted

## Context

The Djinn server's execution coordinator currently:
- Hardcodes `max: 1` concurrent session per model (supervisor.rs:259)
- Picks the first model with `tool_call` capability from the first provider with credentials — no role awareness
- Has no MCP tools to configure execution capacity or model assignment
- Stores model health state only in memory (lost on restart)

The previous Go CLI server solved this with a `settings.json` schema containing `agents` (per-role model priority lists) and `max_sessions` (per-model concurrency limits), plus `settings_get/save/reload/reset` MCP tools. This needs to be ported to the Rust server.

The server has three agent types that benefit from different models:
- **Worker** — implements tasks, writes code (needs strong coding model)
- **Task Reviewer** — reviews individual task PRs (can use cheaper/faster model)
- **Epic Reviewer** — reviews epic-level coherence across tasks (replaces the old "phase reviewer")
- **Conflict Resolver** — resolves merge conflicts (needs coding model)

Note: "Phase Reviewer" from the Go CLI is renamed to **Epic Reviewer** since phases were eliminated (ADR-009) and the review scope is now per-epic.

## Decision

### 1. Settings schema for execution configuration

Add to `~/.djinn/settings.json`:

```json
{
  "agents": {
    "worker": {
      "models": ["synthetic/hf:deepseek-ai/DeepSeek-V3.2", "anthropic/claude-sonnet-4-6"]
    },
    "task-reviewer": {
      "models": ["synthetic/hf:Qwen/Qwen3-235B-A22B-Instruct-2507"]
    },
    "epic-reviewer": {
      "models": ["synthetic/hf:moonshotai/Kimi-K2.5"]
    }
  },
  "max_sessions": {
    "synthetic/hf:deepseek-ai/DeepSeek-V3.2": 3,
    "anthropic/claude-sonnet-4-6": 2
  }
}
```

- `agents.<role>.models` — ordered priority list per agent type. Tried in sequence; first healthy model with capacity wins.
- `max_sessions.<model_id>` — max concurrent sessions per model across all agent types. Default: 1 if not specified.
- Model IDs use `provider_id/model_id` format (e.g., `"synthetic/hf:deepseek-ai/DeepSeek-V3.2"`).

### 2. Four new MCP tools for settings management

- `settings_get` — retrieve current settings (full or by key path)
- `settings_set` — update settings (deep merge into existing)
- `settings_reload` — force reload from disk (after manual file edit)
- `settings_reset` — revert to defaults

### 3. Coordinator reads agent config at dispatch time

The coordinator's `resolve_dispatch_model()` changes from "pick first model with tool_call" to:
1. Determine agent type for task (worker, task-reviewer, epic-reviewer, conflict-resolver)
2. Read `agents.<role>.models` priority list from settings
3. For each model in priority order: check health (not circuit-broken) → check capacity (active < max_sessions) → use it
4. If no configured models available, fall back to any connected model with tool_call capability

### 4. Supervisor reads max_sessions from settings

Instead of hardcoded `max: 1`, the supervisor reads `max_sessions[model_id]` from settings. Missing entries default to 1.

### 5. Model health persisted to settings

Circuit breaker state (consecutive failures, cooldown, auto-disabled) written to settings on change so it survives restarts.

## Consequences

**Positive:**
- Users can assign cheap models to reviewers and strong models to workers
- Per-model concurrency is configurable (run 3 DeepSeek sessions in parallel)
- Settings MCP tools let desktop UI and CLI configure execution without file editing
- Health state survives restarts

**Negative:**
- Settings file becomes a coordination point (file watcher needed for external edits)
- More configuration surface to document and validate

## Relations
- [[decisions/adr-009-simplified-execution-—-no-phases,-direct-task-dispatch|ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — execution model this builds on
- [[V1 Requirements]] — SETTINGS-10 covers the desktop UI for this
- [[Roadmap]] — server-side work, desktop settings UI in Phase 5
