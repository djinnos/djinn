---
title: ADR-025: Backlog Grooming and Autonomous Dispatch Triggers
type: adr
tags: ["adr","dispatch","backlog","triggers","pm","architect"]
---


# ADR-025: Backlog Grooming and Autonomous Dispatch Triggers

**Status:** Accepted
**Date:** 2026-03-06 (revised 2026-03-10)
**Related:** [[ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline]], [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]], [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]]

---

## Context

ADR-024 introduced a PM agent for circuit breaker escalation (firefighter mode). But there's no quality gate before worker dispatch — tasks go from creation straight to `open` and workers pick them up immediately. Underspecified tasks waste tokens when workers fail, get rejected, and loop.

A separate grooming agent that reviews every task before workers touch it pays for itself: the upfront LLM cost of grooming is much less than the wasted tokens from workers flailing on bad tasks.

The "PM" label is a user-facing metaphor. Under the hood, the firefighter PM (ADR-024's `needs_pm_intervention` handler) and the groomer are completely independent agents — different prompts, different triggers, different goals. The only shared trait is "no worktree, read-only shell." They warrant separate `AgentType` variants.

## Decision

### 1. New Agent Type: `Groomer`

A new `AgentType::Groomer` variant, fully separate from `AgentType::PM`.

**Purpose:** Review backlog tasks for AC quality, scope clarity, ADR references, and implementability. Promote ready tasks to `open`. Update underspecified tasks and leave them in `backlog` for re-review.

**Tools:** Same read-heavy set as PM — `task_list`, `task_show`, `task_update`, `task_transition` (Accept only), `task_comment_add`, `memory_read`, `memory_search`, `memory_catalog`, `memory_health`, `shell` (read-only).

**Lifecycle:** No worktree — uses project dir directly (same as PM). Read-only shell.

**Prompt philosophy:** The groomer is spawned with its instructions and uses `task_list(status="backlog")` to see the pile. It processes as many tasks as it can per session. No pre-fetched kickoff context — the groomer pulls what it needs via tools.

### 2. Backlog as Default Task Status

Rename `Draft` → `Backlog` in the state machine. `Backlog` becomes the default status for all `task_create` calls.

**State machine change:**
```
Backlog → (Accept) → Open → (Start) → InProgress → ...
```

The `Accept` transition action is kept as-is (no rename). It means "groomer approves this task for worker dispatch."

**`task_create` change:** Default status becomes `backlog`. An optional `status` parameter allows callers to create tasks directly as `open` — used by the groomer and intervention PM when they create tasks that don't need re-grooming (they already did the quality check).

**Migration:** Renames existing `draft` rows to `backlog` and updates the column default.

### 3. Groomer Dispatch: Debounced Backlog Watch

The coordinator monitors backlog count. When a task enters `backlog` status (via creation or any transition), the trigger fires:

```
Event: task status changed to "backlog"
  → start 2s debounce timer (reset on each new backlog event)
  → when timer fires:
    → if Groomer session is already active: skip
    → if backlog count == 0: skip (tasks were groomed by human in the meantime)
    → else: dispatch Groomer agent

Groomer session completes:
  → check backlog count
  → if backlog > 0: dispatch Groomer again (no debounce, immediate)
  → if backlog == 0: done
```

**Batch strategy:** The groomer gets the full backlog and processes as many tasks as it can. If it runs out of context, compaction kicks in or the session ends and re-spawns since backlog > 0. No artificial batch caps.

**Debounce rationale:** 2 seconds batches rapid task creation bursts (e.g., `/breakdown` creating 10 tasks). Short enough that backlog items don't wait long.

### 4. Groomer Dispatch Priority

Groomer sessions have higher priority than workers. The coordinator dispatches the groomer before new workers when both are eligible. No point dispatching workers if the backlog hasn't been groomed — tasks in `backlog` can't be picked up by workers anyway.

### 5. Model Configuration

The groomer gets its own model config slot via `dispatch_role() -> "groomer"`. Default: same model as workers. Configurable independently via settings (`model_priorities`).

## Consequences

### What Changes

| Component | Current | New |
|-----------|---------|-----|
| Agent types | Worker, TaskReviewer, PM, ConflictResolver | + Groomer |
| Default task status | `open` | `backlog` |
| `Draft` status | Exists, rarely used | Renamed to `Backlog` |
| Worker dispatch | Scan for open+unblocked | Same, but tasks only become open after grooming |
| Groomer dispatch | N/A | Debounced backlog watch + re-spawn loop |
| Dispatch priority | All equal | Groomer > Worker |

### Files Affected

- `src/models/task.rs` — rename `Draft`→`Backlog` in `TaskStatus` enum
- `src/agent/mod.rs` — add `Groomer` to `AgentType`, `dispatch_role`, `for_task_status`
- `src/agent/prompts/groomer.md` — new prompt template
- `src/actors/coordinator/dispatch.rs` — backlog debounce timer, groomer dispatch trigger, priority ordering, re-spawn loop
- `src/actors/slot/lifecycle.rs` — groomer lifecycle (no worktree, same as PM)
- `src/mcp/tools/task_tools/mod.rs` — `task_create` default status to `backlog`, optional `status` param
- Migration — rename `draft`→`backlog` in existing rows, update column default

### What Stays the Same

- PM intervention (ADR-024) — firefighter PM unchanged
- Worker dispatch for `open` tasks — same logic, just tasks arrive via groomer now
- TaskReviewer, ConflictResolver — unchanged
- Session cost tracking, sandboxing, worktrees — unchanged

### Risks

1. **Groomer as bottleneck** — workers idle while groomer processes backlog. Mitigated: groomer prompt encourages fast decisions; re-spawn loop processes everything; groomer model can be cheap/fast.
2. **Cost of grooming well-specified tasks** — human-written tasks with good AC still go through grooming. Mitigated: groomer quickly promotes good tasks (fast LLM call); savings from catching bad tasks outweigh overhead.
3. **Groomer creating circular work** — groomer updates a task, task re-enters backlog. Mitigated: `Accept` transition moves task to `open`, not back to `backlog`. Groomer either promotes or updates-in-place.

### Architect Triggers (Deferred)

ADR-025 originally proposed architect dispatch triggers (merge count threshold). The Architect agent type itself is not yet implemented (ADR-024 scope). Architect triggers will be designed when the Architect is built.

---

## Relations

- [[ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline]] — PM intervention stays separate
- [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]] — circuit breaker feeds PM, not groomer
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — existing dispatch model extended
- [[Roadmap]]