---
title: ADR-036: Structured Session Finalization — Finalize Tools and Forced Tool Choice
type: adr
tags: ["adr","agent-loop","session","tool-choice","roles"]
---


# ADR-036: Structured Session Finalization — Finalize Tools and Forced Tool Choice

**Status:** Accepted
**Date:** 2026-03-18
**Supersedes:** ADR-022 (Outcome-Based Session Validation & Agent Role Redesign)
**Related:** [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]], [[ADR-034: Agent Role Hierarchy — Architect Patrol, Scrum Master Rules, and Task Types]]

---

## Context

ADR-022 removed all text markers and nudging, making "text-only turn" the natural session completion signal. In practice, this created a catastrophic waste problem: **67% of sessions (107/159) ended on the first turn with zero tool calls** — the agent produced only text and the session was considered complete. This is the single largest source of compute waste in autonomous operation.

### Root Causes

1. **Text-only completion is ambiguous.** A text-only response can mean "I'm done" (legitimate) or "I don't know what to do" (failure). The system cannot distinguish them.
2. **Models are trained to conclude.** LLMs default to summarizing and stopping rather than acting. Without structural forcing, the path of least resistance is to narrate a plan and emit text.
3. **No enforcement mechanism.** ADR-022 explicitly removed nudging, leaving no recovery path when the agent fails to act.

### Research Findings

| System | Approach | Enforcement |
|--------|----------|-------------|
| **Goose** | `FinalOutputTool` — session cannot end without calling it; text-only turns trigger nudge continuation | Structural (loop won't exit) |
| **OpenCode** | Aggressive prompt: "NEVER end without solving" | Prompt-only (unreliable) |
| **SWE-Agent** | Every response must contain exactly one action; `FormatError` on text-only | Structural (re-query) |
| **Spotify Honk** | Verifiers as MCP tools + LLM-as-judge veto | Structural (inner loop) |
| **Stripe Minions** | Deterministic lint nodes sandwiched between agentic nodes | Structural (state machine) |

Provider support for `tool_choice: required`:

| Provider | Wire Format | Enforcement Level |
|----------|------------|-------------------|
| **OpenAI** | `"tool_choice": "required"` | Hard — constrained decoding |
| **Anthropic** | `"tool_choice": {"type": "any"}` | Hard — API prompt prefilling |
| **Google Gemini** | `"tool_config": {"function_calling_config": {"mode": "ANY"}}` | Soft — confirmed production violations |
| **Fireworks** | `"tool_choice": "required"` (OpenAI format) | Model-dependent |

---

## Decision

### 1. Every Role Gets a Finalize Tool

Each agent role has a dedicated **finalize tool** that produces structured session output and signals session completion. The session loop does not exit until the finalize tool is called.

| Role | Finalize Tool | Payload | Replaces |
|------|--------------|---------|----------|
| **Worker** | `submit_work` | `{ summary: string, files_changed: string[], remaining_concerns: string[] }` | `task_comment_add` for progress notes |
| **TaskReviewer** | `submit_review` | `{ verdict: "approved" \| "rejected", acceptance_criteria: [{ criterion: string, met: bool }], feedback: string }` | `task_update` for AC + `task_comment_add` for feedback |
| **PM** | `submit_decision` | `{ decision: "reopen" \| "decompose" \| "force_close" \| "escalate", rationale: string, created_tasks: string[] }` | `task_comment_add` + `task_transition` |
| **Groomer** | `submit_grooming` | `{ tasks_reviewed: [{ task_id: string, action: "promoted" \| "improved" \| "skipped", changes: string }] }` | `task_comment_add` |

The finalize tool is **the single source of truth** for session output. The lifecycle code reads the structured payload to drive state transitions, activity log entries, and AC updates — no text parsing.

**Reviewer's `submit_review`** both reports verdicts AND sets the AC met state on the task. This consolidates the current two-step flow (call `task_update` for AC, then emit text verdict) into a single atomic operation.

**Groomer's `submit_grooming`** reports per-task actions since the groomer operates across multiple tasks in a single session. The lifecycle code processes each entry.

### 2. `tool_choice: required` on Every Turn

Set `tool_choice` to "required" (or provider equivalent) on **every turn** of every session. This makes text-only responses structurally impossible at the API level.

Implementation in the provider format layer:

```
// In each format's build_request(), when tools are non-empty:
OpenAI:     body["tool_choice"] = "required"
Anthropic:  body["tool_choice"] = { "type": "any" }
Google:     body["tool_config"] = { "function_calling_config": { "mode": "ANY" } }
Fireworks:  body["tool_choice"] = "required"  (OpenAI format)
```

**Anthropic caveat:** `tool_choice: { type: "any" }` is incompatible with extended thinking (`thinking: { type: "enabled" }`). The provider format layer must omit `tool_choice` when extended thinking is active, falling back to the nudge loop.

### 3. Nudge Loop as Universal Fallback

For providers where `tool_choice` enforcement is unreliable (Gemini, generic Fireworks models) or structurally incompatible (Anthropic with extended thinking), the reply loop implements a **nudge continuation** inspired by Goose's `FinalOutputTool` pattern:

When the reply loop receives a text-only response (no tool calls) AND the finalize tool has not been called:

1. Inject a user message: *"You have not completed your session. You MUST call `{finalize_tool_name}` when you are done. If you still have work to do, use the appropriate tools to continue. If you are done, call `{finalize_tool_name}` now."*
2. Continue the loop.
3. Allow up to **3 nudge attempts**. After 3 consecutive text-only responses, treat as a session failure (provider error equivalent).

This ensures the system works with every provider regardless of `tool_choice` support quality.

### 4. Reply Loop Changes

The reply loop at `crates/djinn-agent/src/actors/slot/reply_loop.rs` changes from:

```
// OLD: text-only turn → break
if turn_tool_calls.is_empty() {
    break;  // session complete
}
```

To:

```
// NEW: check for finalize tool
if turn_tool_calls.iter().any(|tc| tc.name == role.finalize_tool_name()) {
    // Process finalize tool, extract structured payload
    // Break — session complete
    break;
}

// If no finalize tool was called, continue loop normally
// (tool_choice: required prevents text-only turns at API level)
// (nudge loop catches fallback cases)
```

The `saw_any_tool_use` check at line 748 becomes unnecessary — the session always ends via finalize tool or explicit failure.

### 5. Tool Consolidation

With finalize tools capturing structured output, the following tools are **removed from role tool sets**:

| Removed Tool | Was Used By | Replaced By |
|-------------|------------|-------------|
| `task_comment_add` | Worker, PM, Groomer | Finalize tool payloads → activity log |
| `task_update` (AC fields) | TaskReviewer | `submit_review` sets AC atomically |
| `task_transition` | PM | `submit_decision` drives transitions via lifecycle |

These tools remain available for human/CLI use via MCP but are **not registered in agent sessions**.

Workers, reviewers, PM, and groomer each get only the tools relevant to their role plus their finalize tool. The finalize tool is the **only** way to end a session.

### 6. Provider Abstraction: `ToolChoice` Enum

Add a `ToolChoice` enum to the provider abstraction:

```rust
pub enum ToolChoice {
    Auto,      // Provider decides (default when no tools)
    Required,  // Must call at least one tool
    None,      // Must not call tools
}
```

Add `tool_choice: Option<ToolChoice>` as a parameter to `LlmProvider::stream()`. The reply loop passes `ToolChoice::Required` on every turn when tools are present. Each format's `build_request()` maps the enum to the provider-specific wire format.

---

## Consequences

### What Changed

| Component | Before (ADR-022) | After (ADR-036) |
|-----------|-----------------|-----------------|
| Session end signal | Text-only turn (no tool calls) | Finalize tool called |
| Text-only responses | Treated as completion | Impossible (tool_choice: required) or nudged |
| Nudging | Removed entirely | Reinstated as fallback for weak providers |
| Structured output | Free-form text, parsed heuristically | Structured JSON from finalize tools |
| Tool consolidation | Roles use generic task tools | Roles use role-specific finalize tool; generic tools removed |
| `tool_choice` API param | Not used | Set to `required` on every turn |
| Session waste rate | 67% (107/159 sessions) | Expected near-zero (structurally prevented) |

### Positive

- **Eliminates 67% session waste** — text-only first-turn exits become structurally impossible
- **Structured session output** — finalize payloads are machine-readable, no text parsing
- **Single source of truth** — lifecycle code reads finalize payload, not scattered tool calls
- **Works with all providers** — hard enforcement (OpenAI/Anthropic) + nudge fallback (Gemini/others)
- **Simpler role tool sets** — fewer tools per role, clearer boundaries
- **Activity log quality** — entries come from structured payloads, not free-form comments

### Negative

- **Extra API parameter** — `tool_choice` must be threaded through provider abstraction
- **Nudge loop complexity** — fallback path adds code to reply loop
- **Anthropic thinking incompatibility** — must detect and skip `tool_choice` when thinking is enabled
- **Groomer finalize is more complex** — multi-task payload needs careful lifecycle handling
- **Breaking change** — all prompts must be updated to reference finalize tools

### Risks

1. **Agent calls finalize too early** — calls `submit_work` without actually doing work. Mitigated: the reviewer still checks AC against code changes. Prompt instructs to call finalize only after all work is complete.
2. **Agent avoids finalize** — keeps calling other tools forever without finalizing. Mitigated: existing turn limits and compaction thresholds naturally bound session length.
3. **Finalize payload quality** — agent may produce low-quality structured output. Mitigated: schema validation on the finalize tool; reject malformed payloads and nudge.

---

## Implementation Scope

### Provider Layer
- Add `ToolChoice` enum to `crates/djinn-provider/src/provider/mod.rs`
- Add `tool_choice` parameter to `LlmProvider::stream()`
- Map to wire format in `format/{openai,anthropic,google,openai_responses}.rs`

### Reply Loop
- Replace text-only-break with finalize-tool detection
- Add nudge continuation for fallback providers
- Remove `saw_any_tool_use` post-hoc check
- Thread `tool_choice: Required` into every `provider.stream()` call

### Role System
- Add `finalize_tool_name()` to `AgentRole` trait
- Implement finalize tool handlers per role in extension.rs or dedicated module
- Update `RoleConfig` with finalize tool metadata

### Tool Registration
- Register finalize tools per role in tool set construction
- Remove `task_comment_add`, `task_update` (AC), `task_transition` from agent tool sets
- Keep these tools available for human/CLI MCP use

### Prompts
- Update `dev.md` — instruct worker to call `submit_work` when done
- Update `task-reviewer.md` — instruct reviewer to call `submit_review` with AC verdicts
- Update `pm.md` — instruct PM to call `submit_decision`
- Update `groomer.md` — instruct groomer to call `submit_grooming`

### Lifecycle
- Process finalize tool payloads in `on_complete` per role
- Wire `submit_review` to set AC met state atomically
- Wire `submit_decision` to drive task transitions
- Log structured activity from finalize payloads

## Relations

- [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]] — SUPERSEDED
- [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]] — provider abstraction extended
- [[ADR-034: Agent Role Hierarchy — Architect Patrol, Scrum Master Rules, and Task Types]] — new roles will also need finalize tools
- [[Roadmap]] — new phase or integrated into Phase 16 (Operational Reliability)



## Task Breakdown (Epic `52ok`)

- Task `if6o`: Add ToolChoice enum and thread through LlmProvider::stream() — P0 foundation
- Task `bw03`: Define finalize tool schemas and add finalize_tool_name() to AgentRole — P0 foundation
- Task `z525`: Map ToolChoice to wire format in all provider format families — P1, blocked by if6o
- Task `vizi`: Register finalize tools per role, remove generic tools from agent sessions — P1, blocked by bw03
- Task `25gm`: Update role prompts to reference finalize tools — P1, blocked by bw03
- Task `9m22`: Reply loop: finalize-tool detection, nudge loop, remove text-only break — P1, blocked by if6o + bw03 + z525
- Task `95mc`: Finalize tool handlers and lifecycle processing — P2, blocked by vizi + 9m22
