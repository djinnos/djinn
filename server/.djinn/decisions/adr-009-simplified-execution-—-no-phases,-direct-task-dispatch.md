---
title: 'ADR-009: Simplified Execution — No Phases, Direct Task Dispatch'
type: adr
tags: []
---


# ADR-009: Simplified Execution — No Phases, Direct Task Dispatch

## Status: Accepted

## Context

The Go server implements a full phase/step system for agent execution: `work_plans`, `work_plan_phases`, `work_plan_phase_tasks` tables, 26 MCP tools for phase management, a desktop Phase Editor, stacked branches per phase, and phase-level review. This adds significant complexity — phases are the most bug-prone subsystem in the Go server.

The Rust rewrite already decided to merge task branches directly to the target branch (no stacked branches). This eliminates the core reason phases existed: coordinating merges into a shared feature branch. Without stacked branches, phases become pure dispatch grouping — a layer that adds complexity without proportional value.

## Decision

**Remove the phase/step system entirely.** The execution model is:

- A task is dispatchable if: status = `open` AND no unresolved blockers AND model available
- The coordinator dispatches all eligible tasks up to capacity
- Desktop controls execution via start/pause/resume/status — no phase editor
- No dispatch grouping, no ordering beyond priority + creation time
- No max reopen limit — tasks can be reworked indefinitely

The 26-tool execution surface collapses to ~6 tools:
- `execution_start` — enable coordinator dispatch
- `execution_pause(mode)` — graceful (drain) or immediate (WIP commit + stop)
- `execution_resume` — resume dispatch
- `execution_status` — running sessions, capacity, state
- `execution_kill_task(task_id)` — stop specific agent session
- `session_for_task(task_id)` — get session details

**Merge flow:** On task review approval, the supervisor squash-merges to the target branch. If conflicts arise, the task is reopened with conflict metadata and a conflict-resolution agent is dispatched. One conflict at a time via rebase — if clean, keep going; if not, spawn agent.

## Consequences

**Positive:**
- ~20 fewer MCP tools to implement and maintain
- No phase schema (3 tables eliminated)
- No Phase Editor UI complexity in desktop
- Simpler coordinator logic (no phase ordering, no phase transitions)
- Merge conflicts resolved per-task, not per-phase (simpler, no phase-level conflict handling)

**Negative:**
- No dispatch grouping — all ready tasks compete equally (mitigated by priority + blockers)
- No "preview what will happen" — tasks just run when eligible
- Desktop has less control over ordering (can't say "do these 3 first, then those 2")
- Phase-level review (architect batch review) needs rethinking without phases as a grouping unit

## Implementation Clarification (2026-03-03)

- Session capacity is configured per model via `max_sessions` (map of `provider/model` -> integer).
- Total executor capacity is the sum of all configured model capacities.
- Coordinator routing uses per-role model priority lists and attempts fallback models in order when a higher-priority model is at capacity or unavailable.
- Execution controls support both global scope and project scope: global start dispatches across all currently registered projects, while project-scoped start/pause/resume only affects that project.
- Stuck-session recovery releases tasks with no active session across `in_progress` and `in_task_review` so they can be re-dispatched automatically. Epic review batch recovery is tracked separately via persisted batch status (`queued`/`in_review`).
- Task branches are local by default; creating/pushing remote `task/*` branches is optional and not required for dispatch.
- After any session ends, supervisor triggers immediate project-scoped dispatch to start the next ready task without waiting for coordinator tick.
- Task-review merges run from a detached temporary merge worktree and push directly to `origin/<target>` to avoid mutating the user's local checked-out branch.

## Relations

- [[Roadmap]] — Eliminates Phase system from all phases
- [[V1 Requirements]] — Simplifies AGENT-01, AGENT-07; removes implicit phase requirements
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — Complements in-process model
