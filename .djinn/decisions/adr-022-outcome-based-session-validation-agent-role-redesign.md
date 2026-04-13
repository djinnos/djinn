
# ADR-022: Outcome-Based Session Validation & Agent Role Redesign

**Status:** Accepted
**Date:** 2026-03-06
**Updated:** 2026-03-08
**Supersedes:** ADR-012 (Epic Review Batches and Structured Output Nudging), output parsing markers in `lypu` (Phase 9)
**Related:** [[decisions/adr-009-simplified-execution-—-no-phases,-direct-task-dispatch|ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]]

---

## Context

The original system used text markers (`WORKER_RESULT: DONE`, `REVIEW_RESULT: VERIFIED/REOPEN`, `EPIC_REVIEW_RESULT: CLEAN/ISSUES_FOUND`) as the sole signal for agent completion and routing, with a post-session nudge mechanism when markers were missing. This was unreliable:

1. **Workers emit DONE without implementing anything.** Models explore the codebase, narrate a plan, then emit DONE without calling write/edit tools. The reviewer rejects, creating infinite loops.
2. **The nudge makes it worse.** When the marker is missing, nudging "continue implementing" just causes the model to emit DONE again — it has mentally checked out.
3. **DONE is a perverse incentive.** Models are trained to conclude conversations. Emitting DONE is the path of least resistance.
4. **Markers are fragile.** Parsing text for structured signals is error-prone and adds complexity that provides no real value.

### Reference Systems Studied

**ralph-claude-code** uses git diff as ground truth — no single marker is authoritative. **gastown** uses structural role separation and observable state ("discover, don't track").

---

## Decision

### 1. Workers: Session End = Completion

**All markers removed.** The model stopping tool calls is the natural completion signal. When the reply loop ends normally (no cancellation, no error), the worker is considered done. The task always proceeds to review.

No nudging. No markers. No `WORKER_RESULT`. The agent works until it's done, then stops.

### 2. Reviewers: AC-Driven Verdicts

**Replace text marker routing with acceptance criteria state.**

- Reviewer MUST call `task_update(id, acceptance_criteria=[...])` with each criterion marked `met: true` or `met: false`.
- **All ACs met** → approved. Task proceeds to merge.
- **Any AC not met** → rejected. Task sent back to worker with feedback.
- **Reviewer doesn't update ACs** → conservative default: all unmet → reject.

No `REVIEW_RESULT` markers. The AC state is the only routing signal.

### 3. Epic Reviewer: Removed

The EpicReviewer agent type is eliminated entirely. Epic-level quality is handled by task-level review — each task is individually reviewed against its acceptance criteria. Epic lifecycle (close when all tasks closed) is handled by the existing epic state machine without a dedicated reviewer agent.

### 4. No Nudging

All nudging is removed. The post-session nudge mechanism added complexity and didn't improve outcomes — models that failed to produce work on the first pass rarely improved with a nudge. Instead:

- If a session ends without any tool use → treated as a provider error (session fails).
- If a session ends with tool use but no file changes → the reviewer will catch this via unmet AC.
- The worker→reviewer→reopen loop provides natural retry with feedback.

### 5. Circuit Breaker (Future)

Track consecutive failed attempts per task:

| Condition | Action |
|-----------|--------|
| Task reopened 3+ times by reviewer | Mark task `failed`, reason: "exceeded reopen limit" |
| Worker session errors 3+ times consecutively | Mark task `failed`, reason: "consecutive session failures" |

## Consequences

### What Changed

| Component | Before | After |
|-----------|--------|-------|
| Worker completion signal | `WORKER_RESULT: DONE` text marker | Session ends naturally |
| Reviewer verdict | `REVIEW_RESULT: VERIFIED/REOPEN` text | AC `met` state on each criterion |
| Epic reviewer | Separate agent type with `EPIC_REVIEW_RESULT` | Removed entirely |
| Nudging | Post-session nudge on missing marker | Removed entirely |
| No-tool-use sessions | Missing marker error | Provider error (session fails) |
| Output parser | Parsed 3 marker types + nudge helpers | Only extracts runtime errors + feedback text |

### Files Changed

- `src/agent/output_parser.rs` — stripped to runtime error + feedback extraction only
- `src/agent/mod.rs` — removed `EpicReviewer` from `AgentType` enum
- `src/agent/config.rs` — removed `epic_reviewer()` constructor
- `src/agent/extension.rs` — removed EpicReviewer tool branch
- `src/agent/prompts.rs` — removed epic reviewer template, removed batch fields from `TaskContext`
- `src/agent/prompts/dev.md` — removed WORKER_RESULT marker requirement
- `src/agent/prompts/task-reviewer.md` — AC-driven verdict, no REVIEW_RESULT marker
- `src/agent/prompts/conflict-resolver.md` — removed WORKER_RESULT marker
- `src/agent/prompts/epic-reviewer-batch.md` — deleted
- `src/actors/slot/reply_loop.rs` — removed nudge logic, removed marker checks
- `src/actors/slot/helpers.rs` — removed `missing_required_marker`, `missing_marker_nudge`, EpicReviewer branches
- `src/actors/slot/epic_review.rs` — `success_transition()` uses AC state; removed `finalize_epic_batch`
- `src/actors/slot/lifecycle.rs` — removed EpicReviewer dispatch, batch tracking, worker_signal checks

### What Stays the Same

- Compaction handled by Goose internally
- Session cost tracking and token metrics
- Goose reply loop mechanics
- Worktree lifecycle (create, commit, cleanup)
- Verification commands (setup + verify still run before reviewer)
- Worker→reviewer→reopen loop for natural retry

### Risks

1. **Reviewer doesn't update ACs** — defaults to reject (conservative), which is safe but could cause unnecessary rework. Mitigated: prompt is explicit and mandatory.
2. **Worker produces no meaningful changes** — reviewer catches via unmet AC and rejects. Natural loop handles retry.

---

## Relations

- [[decisions/adr-009-simplified-execution-—-no-phases,-direct-task-dispatch|ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — execution model unchanged
- [[V1 Requirements]] — extends REVIEW-01, REVIEW-03, AGENT-08 (stuck detection)
