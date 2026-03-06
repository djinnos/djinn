---
title: ADR-025: Backlog Grooming and Autonomous Dispatch Triggers
type: adr
tags: ["adr","dispatch","backlog","triggers","pm","architect"]
---

# ADR-025: Backlog Grooming and Autonomous Dispatch Triggers

**Status:** Proposed
**Date:** 2026-03-06
**Related:** [[ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline]], [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]], [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]]

---

## Context

ADR-024 introduces PM and Architect agent types. Both need autonomous dispatch triggers — they're not dispatched by the coordinator's existing "find open task, assign worker" loop. The PM grooms the backlog before workers can be dispatched. The Architect analyzes the codebase proactively. Both need clear triggering rules to avoid over-dispatching (wasting compute) or under-dispatching (letting backlog/debt accumulate).

The existing dispatch model (ADR-009) is event-driven: coordinator scans for `open` + unblocked tasks and dispatches workers. PM and Architect need different triggers tied to system state changes, not task readiness.

## Decision

### 1. Backlog as Default Task Status

Tasks created by agents (PM, Architect) default to `backlog` status. Tasks created by humans via MCP tools also default to `backlog` unless explicitly set otherwise.

**State machine addition:**
```
Backlog → (pm_approve) → Open → (start) → InProgress → ...
```

The existing `Draft` status is repurposed: `Draft` is renamed to `Backlog` in the state machine. The `Accept` transition action is renamed to `PmApprove`. This is a rename, not a new state — the DB migration updates existing `draft` values to `backlog`.

**`task_create` default:** The MCP `task_create` tool sets `status = backlog` by default. The frontend tool (used by agents) also defaults to `backlog`. No task enters `open` without passing through PM grooming or explicit human override (`UserOverride` transition).

### 2. PM Trigger: Debounced Backlog Watch

The coordinator monitors the backlog count. When a task enters `backlog` status (via creation or circuit breaker transition), the trigger fires:

```
Event: task status changed to "backlog"
  → start 2s debounce timer (reset on each new backlog event)
  → when timer fires:
    → if PM session is already active: skip
    → if backlog count == 0: skip (tasks were groomed by human in the meantime)
    → else: dispatch PM agent

PM session completes:
  → check backlog count
  → if backlog > 0: dispatch PM again (no debounce, immediate)
  → if backlog == 0: done
```

**Debounce rationale:** 2 seconds is long enough to batch multiple task creations (e.g., architect creating 5 tasks in rapid succession) but short enough that backlog items don't wait long. The re-spawn loop ensures the PM processes everything even if a single session only handles a subset.

**PM prompt guidance:** "You are reviewing the backlog. Process tasks in priority order. For each task: verify it has clear AC, proper ADR references, appropriate scope. If ready, transition to `open`. If underspecified, update the task with missing details then transition. If it references a Proposed ADR, leave it in backlog with a comment. Don't try to process everything — you'll be re-spawned if backlog remains."

**Circuit breaker trigger:** When ADR-022's circuit breaker transitions a task to `pm_review`, this is treated as a backlog event for PM dispatch purposes. The PM receives the failed task with full activity log context (session errors, reviewer feedback, reopen history).

### 3. Architect Trigger: Codebase Change Threshold

The coordinator tracks codebase change volume since the last architect run. The trigger is commit-based:

```
After each successful task merge (squash-merge to target branch):
  → increment tasks_merged_since_last_architect counter
  → if counter >= ARCHITECT_THRESHOLD (default: 10):
    → if Architect session is already active: skip
    → else: dispatch Architect agent
    → reset counter

Architect session completes:
  → counter already reset when dispatched
  → any tasks created by architect land in backlog → PM trigger fires
```

**Threshold rationale:** 10 merged tasks represents meaningful codebase change — enough to warrant a fresh analysis but not so frequent that the architect runs constantly. This is configurable via settings (`architect_merge_threshold`).

**Alternative triggers (future, not in v1):**
- Time-based with activity gate (e.g., weekly if ≥5 tasks merged)
- Lines-of-code changed threshold
- Human-initiated via `execution_start` with architect type

**Architect prompt guidance:** "Analyze the codebase against existing Accepted ADRs and pattern notes. For each violation found, create a task in backlog referencing the ADR. For new architectural concerns, write a Proposed ADR — do not create tasks for proposals. Check KB health: cross-reference ADR content against actual code, flag superseded decisions, update pattern notes where implementation diverged. Prioritize high-impact findings over cosmetic issues."

### 4. Continuation Context in Kickoff Messages

Both PM and Architect benefit from knowing their dispatch context. The kickoff message includes:

**PM kickoff:**
```
## Dispatch Context
Trigger: backlog_watch | circuit_breaker
Backlog count: {N}
Session: {continuation_number} of this grooming cycle

## KB Health
Coherence metrics from memory_health
Orphan count, broken links

## Catalog
Full memory_catalog output

## Backlog Items (first 20)
[task summaries with short_id, title, priority, who created them]
```

**Architect kickoff:**
```
## Dispatch Context
Trigger: merge_threshold
Tasks merged since last run: {N}
Recent merge commits: [list of merged task short_ids and titles]

## KB Health
Coherence metrics from memory_health

## Catalog
Full memory_catalog output

## Current ADRs (Accepted)
[list of accepted ADR titles for enforcement reference]
```

### 5. Capacity and Priority

PM and Architect sessions use the existing slot pool (ADR-008). They don't have dedicated slots — they compete with workers for capacity.

**Priority:** PM sessions have higher priority than workers when the backlog is non-empty. The coordinator should dispatch PM before new workers when both are eligible. This prevents the pathological case where workers consume all slots while the backlog grows unbounded.

**Model selection:** PM and Architect can use different models than workers. The prompt-to-model mapping is configurable via settings. Default: PM uses the same model as workers; Architect uses a higher-capability model (more reasoning needed for codebase analysis).

## Consequences

### What Changes

| Component | Current | New |
|-----------|---------|-----|
| Default task status | `open` (agent) / `draft` (MCP) | `backlog` for all |
| `Draft` status | Exists, rarely used | Renamed to `Backlog` |
| `Accept` transition | Anyone can use | Renamed to `PmApprove` |
| Worker dispatch | Scan for open+unblocked | Same, but tasks only become open after PM grooming |
| PM dispatch | N/A | Debounced backlog watch + circuit breaker |
| Architect dispatch | N/A | Codebase change threshold (default: 10 merges) |
| Dispatch priority | All equal | PM > Worker when backlog non-empty |

### Files Affected

- `src/models/task.rs` — rename `Draft`→`Backlog`, `Accept`→`PmApprove`, add `PmReview`+`Decomposed` states
- `src/actors/coordinator/dispatch.rs` — add PM/Architect trigger logic, priority ordering
- `src/actors/coordinator/mod.rs` — add `tasks_merged_since_last_architect` counter, backlog debounce timer
- `src/actors/slot/lifecycle.rs` — PM/Architect session lifecycle (no worktree, read-only shell)
- `src/agent/config.rs` — PM/Architect model selection
- `src/mcp/tools/task_tools/mod.rs` — default status to `backlog` in `task_create`
- Migration — rename `draft`→`backlog` in existing rows

### Risks

1. **PM as dispatch bottleneck** — if PM is slow, workers starve waiting for open tasks. Mitigated: PM prompt encourages fast decisions; PM can be re-spawned in parallel if capacity allows.
2. **Architect threshold too low** — runs too often, creates noise. Mitigated: configurable threshold; PM filters low-value tasks.
3. **Architect threshold too high** — drift accumulates before detection. Mitigated: human can trigger architect on-demand via execution tools.
4. **Debounce too short** — PM dispatched for single tasks. Mitigated: 2s debounce batches typical creation bursts; PM prompt handles small batches efficiently.

---

## Relations

- [[ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline]] — agent types and tools
- [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]] — circuit breaker → PM dispatch
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — existing dispatch model extended
- [[Roadmap]] — adds to Phase 10