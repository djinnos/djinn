---
title: Adr 041 Unified Tool Service Layer In Djinn Mcp
type: adr
tags: []
---

# ADR-041: Unified Tool Service Layer in djinn-mcp

**Status:** Accepted
**Date:** 2026-03-22
**Authors:** Fernando, Claude

## Context

Tool implementations are fully duplicated between two independent stacks:

- **djinn-mcp** (MCP layer): serves the desktop app and external MCP clients via `rmcp` framework
- **djinn-agent extension** (extension layer): serves internal agents (architect, worker, etc.) during task sessions

15 tools exist in both layers with zero code sharing. Each independently defines param structs, validation, DB queries, response formatting, and JSON schemas. Bugs must be fixed twice, and new tools require parallel implementations.

### Current dependency graph

```
djinn-core  ←── djinn-db  ←── djinn-mcp
                    ↑
                    └───── djinn-agent
```

djinn-agent and djinn-mcp are fully isolated — no imports between them. All shared model types (`Agent`, `Task`, `Epic`, etc.) live in djinn-core/djinn-db.

### Duplicated tools (15)

| Category | Tools |
|----------|-------|
| Tasks | task_list, task_show, task_create, task_update, task_activity_list, task_comment_add, task_transition, task_blocked_list |
| Epics | epic_show, epic_update, epic_tasks |
| Memory | memory_read, memory_search, memory_list, memory_build_context |
| Agents | agent_metrics, agent_create |

### Extension-only tools (13)

Worktree-scoped: shell, read, write, edit, apply_patch, lsp
Agent-specific: request_lead, request_architect, task_kill_session, task_delete_branch, task_archive_activity, task_reset_counters, task_update_ac

### MCP-only tools (38+)

Project management, execution control, memory writes/edits, system diagnostics, board health, session management, etc.

## Decision

**Introduce an `ops` service layer within djinn-mcp** and make djinn-agent depend on djinn-mcp for shared tool logic.

### Architecture

Each tool module in djinn-mcp gains an `ops.rs` file containing framework-free business logic:

```
djinn-mcp/src/tools/
├── task_tools/
│   ├── ops.rs          ← pure business logic: async fn(db, params) -> Result<T>
│   ├── mod.rs          ← MCP adapter: #[tool] fn → calls ops, wraps in Json<>
│   └── types.rs        ← shared param/response types
├── epic_tools/
│   ├── ops.rs
│   └── mod.rs
├── memory_tools/
│   ├── ops.rs
│   └── ...
├── agent_tools/
│   ├── ops.rs
│   └── mod.rs
```

The `ops.rs` functions:
- Accept `Database` (or repos) + a plain params struct
- Return `Result<T>` where T is a serializable response type
- Have **no** rmcp, `Json<>`, or framework dependencies
- Contain all validation, querying, and response construction

### Updated dependency graph

```
djinn-core  ←── djinn-db  ←── djinn-mcp (owns ops layer)
                    ↑              ↑
                    └───── djinn-agent (depends on djinn-mcp for ops)
```

No cycle: djinn-mcp depends on djinn-db/djinn-core. djinn-agent adds djinn-mcp as a dependency. djinn-mcp does not depend on djinn-agent.

### Extension layer changes

The extension dispatch in djinn-agent shrinks from ~3000 lines to thin adapters:

```rust
// Before: full reimplementation
async fn call_task_list(state, args) -> Result<Value, String> {
    let p: TaskListParams = parse_args(args)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    // ... 40 lines of query building, pagination, response formatting
}

// After: thin adapter calling ops
async fn call_task_list(state, args) -> Result<Value, String> {
    let p: TaskListParams = parse_args(args)?;
    let result = djinn_mcp::tools::task_tools::ops::list_tasks(&state.db, p).await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(result).unwrap())
}
```

Extension-only tools (shell, read, write, edit, lsp, agent escalations) remain in djinn-agent unchanged.

### Schema unification

Tool schemas currently exist in two places:
- `djinn-agent/src/extension/schemas.rs` and `mod.rs` (hand-rolled `RmcpTool` structs)
- `djinn-mcp/src/tools/*/mod.rs` (schemars-derived via `#[tool]` macro)

After migration, the extension layer can re-export schemas from djinn-mcp or generate them from the shared types, eliminating the second set of hand-rolled schemas for shared tools.

## Migration strategy

Incremental, one tool group at a time:

1. **Task tools first** (largest group, 8 tools) — extract `ops.rs`, add djinn-mcp dep to djinn-agent, convert extension handlers
2. **Epic tools** (3 tools)
3. **Memory tools** (4 tools)
4. **Agent tools** (2 tools)

Each step is a self-contained PR. Existing tests in both layers continue to work — MCP tests call `#[tool]` handlers, extension tests call dispatch, both ultimately hit `ops`.

## Consequences

### Positive
- Single source of truth for business logic — bugs fixed once
- New shared tools only implemented once in `ops.rs`
- Extension layer becomes ~300 lines of dispatch instead of ~3000
- Consistent validation and error messages across both surfaces
- Shared param/response types reduce type proliferation

### Negative
- djinn-agent gains a dependency on djinn-mcp (acceptable: no cycle, djinn-mcp is a leaf crate)
- djinn-mcp's `ops` functions become a public API surface that both crates depend on
- Migration requires touching all 15 duplicated tool handlers

### Neutral
- MCP-only and extension-only tools are unaffected
- No changes to the MCP protocol or desktop app
- No changes to agent behavior — same tools, same semantics
