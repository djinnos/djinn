---
title: Structured Session Finalization Scope
type: reference
tags: ["scope","reference","adr-036"]
---

# Structured Session Finalization Scope — ADR-036

## In Scope

### Provider Layer (ToolChoice enum)
- `ToolChoice` enum (`Auto`, `Required`, `None`) in `crates/djinn-provider/src/provider/mod.rs`
- Add `tool_choice: Option<ToolChoice>` parameter to `LlmProvider::stream()` signature
- Wire format mapping in `format/openai.rs` → `"tool_choice": "required"`
- Wire format mapping in `format/anthropic.rs` → `"tool_choice": {"type": "any"}`
- Wire format mapping in `format/google.rs` → `"tool_config": {"function_calling_config": {"mode": "ANY"}}`
- Wire format mapping in `format/openai_responses.rs` → `"tool_choice": "required"`
- Anthropic extended thinking detection: omit `tool_choice` when thinking is enabled
- Update `client.rs` call site and `chat.rs` call site

### Reply Loop (finalize detection + nudge)
- Replace text-only-break at `reply_loop.rs:570-579` with finalize tool detection
- Add finalize tool name to role config / AgentRole trait
- Detect finalize tool in tool call list → extract payload → break
- Nudge loop: on text-only turn without finalize, inject nudge user message, continue (max 3 attempts)
- Remove `saw_any_tool_use` post-hoc check at line 748
- Thread `ToolChoice::Required` into every `provider.stream()` call from reply loop

### Finalize Tools (4 tools)
- `submit_work` — Worker finalize. Payload: summary, files_changed, remaining_concerns. Lifecycle: log activity entry from structured payload.
- `submit_review` — TaskReviewer finalize. Payload: verdict (approved/rejected), acceptance_criteria (with met state), feedback. Lifecycle: atomically set AC met state on task, drive approved→merge or rejected→reopen transition.
- `submit_decision` — PM finalize. Payload: decision type (reopen/decompose/force_close/escalate), rationale, created_tasks. Lifecycle: drive task transition based on decision type.
- `submit_grooming` — Groomer finalize. Payload: tasks_reviewed array (task_id, action, changes). Lifecycle: process each entry, log activity per task.

### Tool Consolidation (removals from agent sessions)
- Remove `task_comment_add` from all agent role tool sets
- Remove `task_update` AC fields from reviewer tool set
- Remove `task_transition` from PM tool set
- Keep all tools available for human/CLI MCP access

### Prompt Updates
- `dev.md` — instruct worker to call `submit_work` when done; remove references to "session ends when you stop calling tools"
- `task-reviewer.md` — instruct reviewer to call `submit_review` with AC verdicts; remove instructions about calling `task_update` for AC
- `pm.md` — instruct PM to call `submit_decision`; remove instructions about `task_transition` and `task_comment_add`
- `groomer.md` — instruct groomer to call `submit_grooming` with per-task results

### Role System
- Add `fn finalize_tool_name(&self) -> &str` to `AgentRole` trait
- Add `fn finalize_tool_schema(&self) -> Value` to `AgentRole` trait (or RoleConfig)
- Update `RoleConfig` with finalize tool metadata
- Update `on_complete` per role to process finalize payload

## Out of Scope

- **In-session verification** (Spotify Honk inner-loop pattern) — separate concern, may be Phase 16 or its own ADR
- **Tiered verification** (cargo check → clippy → test) — separate from session finalization
- **ADR-034 new roles** (Architect, Lead/Planner renames) — those roles will get finalize tools when they're implemented
- **Chat endpoint** — `POST /api/chat/completions` does not use finalize tools (interactive, not autonomous)
- **Vector search / RAG** — unrelated

## Preferences

- Finalize tool handlers should live in a dedicated module (e.g., `crates/djinn-agent/src/finalize/`) rather than being mixed into extension.rs
- Finalize tool schemas should use serde for payload (de)serialization, not manual JSON parsing
- Nudge messages should be simple and direct, not elaborate
- Keep the ToolChoice enum minimal (3 variants), don't add ToolChoice::Specific(name) yet

## Relations

- [[ADR-036: Structured Session Finalization — Finalize Tools and Forced Tool Choice]]
- [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]] — superseded
- [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]] — provider abstraction extended
- [[roadmap]]
