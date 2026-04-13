---
tags:
    - adr
    - observability
    - cost-tracking
title: 'ADR-010: Session Cost Tracking — Per-Task Token Metrics'
type: adr
---
# ADR-010: Session Cost Tracking — Per-Task Token Metrics

## Status: Accepted

## Context

The Go server tracks active sessions in memory but does not persist session history with token/cost metrics. Visibility into agent costs — per task, per project, per run — is essential for understanding resource consumption and optimizing model selection.

Goose already tracks token usage internally via its `SessionManager` at `~/.djinn/sessions/`. However, this data lives in a separate SQLite DB, is tied to Goose's session lifecycle, and doesn't survive session cleanup.

## Decision

**Store session metadata in the Djinn DB on session completion.** A `sessions` table records every agent session with token metrics:

```sql
sessions (
  id TEXT PRIMARY KEY,           -- UUIDv7
  task_id TEXT NOT NULL,         -- FK to tasks
  model_id TEXT NOT NULL,        -- e.g., "anthropic/claude-sonnet-4-5"
  agent_type TEXT NOT NULL,      -- worker, task_reviewer, epic_reviewer
  started_at TEXT NOT NULL,
  ended_at TEXT,
  status TEXT NOT NULL,          -- running, completed, interrupted, failed
  tokens_in INTEGER DEFAULT 0,
  tokens_out INTEGER DEFAULT 0,
  worktree_path TEXT,
  FOREIGN KEY (task_id) REFERENCES tasks(id)
)
```

- On session completion (or interruption/failure), the supervisor writes session data to the Djinn DB
- Token counts pulled from Goose session metrics at completion time
- A task may have multiple sessions (initial work, review, conflict resolution, reopens)
- MCP tool: session info visible via `task_show` (last/active session) and aggregate metrics
- Events: `SessionCompleted` broadcast for desktop real-time updates

## Consequences

**Positive:**
- Cost visibility per task, per project, per model
- Desktop can show "this task used X tokens across Y sessions"
- Foundation for cost governance (ACU budgets) in v2
- Session history survives Goose session cleanup

**Negative:**
- Additional DB writes on every session completion
- Token counts are approximate (Goose may not track exact provider-reported usage)
- Schema migration needed

## Relations

- [[roadmap]] — New requirement for observability
- [[requirements/v1-requirements]] — Extends OBS-01, foundation for AGENT-15 (v2)
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — Goose provides the raw metrics
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — Sessions tracked per task, not per phase
