---
title: ADR-046: Chat-Driven Planning — Drafting Epics, Research Agent Deliverables, and Memory Write Access
type: adr
tags: ["chat","planning","research","memory","architect","planner","epic"]
---

# ADR-046: Chat-Driven Planning — Drafting Epics, Research Agent Deliverables, and Memory Write Access

## Status

Accepted

## Context

### The planning workflow today

Djinn's planning workflow operates through Claude Code plugin skills (`/init-project`, `/planning`, `/breakdown`) and the server-side Planner role. The intended flow is:

1. User runs `/planning` in Claude Code → adaptive discussion → ADRs + scope notes
2. User runs `/breakdown` → tasks created under epics with blockers
3. `execution_start` → Coordinator dispatches agents

The desktop chat experience (Phase 13, 80% complete) will become the primary planning interface — replacing the plugin skills for most users. Chat has full MCP tool access and can create epics, tasks, and memory notes.

### Problem 1: Epic creation triggers immediate Planner dispatch

When `epic_create` is called, the Coordinator's event handler in `wave.rs` auto-creates a `planning` task at `PRIORITY_CRITICAL` and dispatches the Planner immediately. This is correct for autonomous operation but wrong during the chat thinking phase, where the human is actively shaping the epic through research, discussion, and ADR writing.

There is no way to create an epic without the Planner jumping in.

### Problem 2: Research agents can't write deliverables

The Architect (handling spikes) and Worker (handling research tasks) cannot write to the knowledge base — `memory_write` and `memory_edit` are not in their tool schemas. This means:

- **Spike findings evaporate.** The Architect's `submit_work` payload goes to the task activity log but not to memory. The Planner must manually call `task_activity_list()` to find them.
- **Research task findings are generic.** Post-session LLM extraction creates cases/patterns/pitfalls at confidence 0.5, but these are automated extractions — not intentional research deliverables.
- **The Planner starts from scratch.** When the Planner creates a planning task for an epic, it gets the epic's `memory_refs` and can call `build_context()`. But if no research agent wrote findings to memory, there's nothing to find.

### Problem 3: No auto-linking of findings to epic context

Even if agents could write memory notes, those notes wouldn't automatically appear in the epic's `memory_refs`. The Planner's auto-created planning task includes the epic's memory_refs in its design field — but research findings written during spike/research tasks are disconnected from the epic.

### Problem 4: Specialist agents lack external tool access

The agent library at `/home/fernando/git/djinnos/agents/agents/` defines specialists with WebSearch/WebFetch capabilities (knowledge-harvester, competitive-analyzer, market-researcher). These are Claude Code subagent definitions today. Djinn's specialist agent system (`mcp_servers` field on agents) is scaffolded but not wired — `resolved_mcp_servers` is loaded in lifecycle.rs but never connected to tool schemas or dispatch.

## Decision

### 1. Epic `drafting` status

Add `drafting` as an epic status alongside `open` and `closed`.

- `epic_create` defaults to `drafting` (new behavior) unless `status: "open"` is explicitly passed
- `maybe_create_planning_task()` in `wave.rs` skips epics with `status != "open"`
- `epic_update(status: "open")` on a drafting epic triggers the planning task creation
- The coordinator treats `drafting` epics as invisible for dispatch purposes

This allows chat to create and populate epics without Planner interference. The user explicitly promotes to `open` when ready for automated decomposition — or skips the Planner entirely by creating all tasks themselves.

### 2. `memory_write` and `memory_edit` for Architect and Worker roles

Add `memory_write` and `memory_edit` to the tool schemas for:
- **Architect** (`tool_schemas_architect()`) — for spike findings, tech spike notes, ADR drafts
- **Worker** (`tool_schemas_worker()`) — for research findings on research issue-type tasks

Both already have `memory_read`, `memory_search`, `memory_list`. Adding write access completes the cycle.

Update prompts:
- **Architect prompt** (`architect.md`): "When completing a spike, write your findings to memory as a `tech_spike` or `research` note. Include the task short_id in the note for traceability."
- **Worker prompt** (`dev.md`): Add a conditional section for research tasks: "Your deliverable is a memory note with your findings, not code. Write findings using `memory_write(type='research', ...)`."

### 3. Auto-link memory notes to epic on task close

When a task closes and has `memory_refs` pointing to notes it created during the session:
- Append those note permalinks to the parent epic's `memory_refs` (if not already present)
- This happens in the task transition handler, not in the agent

Additionally, extend post-session structural extraction to detect `memory_write` calls and auto-add the resulting note permalinks to both:
- The task's `memory_refs`
- The parent epic's `memory_refs`

This ensures the Planner's auto-created planning task naturally includes research findings in its design field context.

### 4. MCP server wiring for specialist agents

Complete the scaffolded but unconnected MCP server integration:

1. At session start, query each `resolved_mcp_server` for its tool definitions via the MCP protocol
2. Convert MCP tool schemas to JSON schemas and append to the session's `tools` array
3. Add a fallback in `dispatch_tool_call` that routes unknown tool names to the appropriate MCP server
4. Handle MCP tool results and errors within the reply loop

This enables specialist agents to use external tools (WebSearch, WebFetch, GitHub API) configured in `.djinn/settings.json`.

### 5. No new agent roles

The existing role hierarchy is sufficient:
- **Worker** handles `research` issue type (simple lifecycle, now with memory_write)
- **Architect** handles `spike` issue type (simple lifecycle, now with memory_write)
- **Planner** decomposes epics into task waves (reads research findings from memory)
- **Chat** acts as the human-in-the-loop planner during the drafting phase

Specialist agents from the agent library become Djinn specialists via `agent_create(base_role="worker", mcp_servers=["web-search"], ...)` once MCP wiring is complete.

### 6. No singleton roadmap required

The epic list replaces the roadmap:
- `drafting` epics = pipeline of planned work (being shaped)
- `open` epics = committed work (being executed)
- `closed` epics = completed work

The ADR trail captures decision history. Research notes capture exploration. Per-epic planning notes capture decomposition rationale. The singleton `roadmap.md` becomes optional — a human convenience, not a system artifact.

## Consequences

### Positive

- **Chat-as-planner workflow unblocked.** Users can create epics, dispatch research, iterate on design, and hand off to automated Planner when ready.
- **Research deliverables persist.** Spike and research findings written to memory survive task closure and feed into Planner context.
- **Planner gets richer context.** Auto-linked memory_refs mean the Planner's planning task includes all research findings without manual linking.
- **Agent library unlocked.** Specialist agents with external tool access enable parallel deep research across multiple topics.
- **Simpler mental model.** No roadmap to maintain. Epic statuses tell the whole story.

### Negative

- **`memory_write` for Workers increases note volume.** More agents writing notes means more potential duplicates. Mitigated by ADR-045's content hash dedup gate and post-session consolidation.
- **`drafting` status adds complexity to epic lifecycle.** One more state to track. Mitigated by defaulting to `drafting` — users only see `open` when they explicitly promote.
- **MCP server wiring is non-trivial.** Connecting external tool servers adds latency and failure modes to agent sessions. Mitigated by health tracking and graceful fallback.

## Alternatives Considered

### Dedicated "Researcher" agent role

A new base role specifically for research tasks. Rejected: the Worker role with `memory_write` and specialist MCP servers is sufficient. Adding a role increases dispatch complexity without clear benefit.

### Epic `hold` field instead of status

A boolean flag `hold: true` on epics instead of a new status. Rejected: a status is more visible in the UI and follows the existing status pattern. `drafting` communicates intent better than a hidden flag.

### Chat does all research inline (no agent dispatch)

Chat handles research via its own tool calls without creating tasks. Rejected for deep research: chat is single-threaded and burns expensive context. Parallel research agents with isolated context windows are faster and cheaper for multi-topic investigation.

### Automatic Planner skip when chat is active

Detect that a chat session is working on the epic and suppress Planner dispatch. Rejected: fragile heuristic. Explicit `drafting` status is simpler and more predictable.

## Relations

- [[ADR-034: Agent Role Hierarchy — Architect Patrol, Task Types, and Escalation]] — Architect role extended with memory_write
- [[ADR-038: Configurable Agent Roles, Domain Specialists, and Auto-Improvement]] — specialist agents gain MCP server tools
- [[ADR-045: SSE Event Batching and Knowledge Base Housekeeping]] — handles increased note volume from memory_write access
- [[ADR-042: DB-Only Knowledge Extraction, Consolidation, and Task Routing Fixes]] — extraction pipeline extended
- [[roadmap]] — replaces roadmap as primary planning artifact
