
# ADR-022: Outcome-Based Session Validation & Agent Role Redesign

**Status:** Proposed
**Date:** 2026-03-06
**Supersedes:** Partially supersedes output parsing in `lypu` (Phase 9)
**Related:** [[ADR-012 Epic Review Batches and Structured Output Nudging]], [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]]

---

## Context

The current system uses text markers (`WORKER_RESULT: DONE`, `REVIEW_RESULT: VERIFIED/REOPEN`) as the sole signal for agent completion and routing. This has proven unreliable:

1. **Workers emit DONE without implementing anything.** Models (especially non-Anthropic) explore the codebase, narrate a plan, then emit DONE without calling write/edit tools. The reviewer correctly rejects, creating an infinite worker→reviewer→reopen loop.
2. **The nudge makes it worse.** When the marker is missing, we nudge "continue implementing" — but the model has already mentally checked out. It just emits DONE again.
3. **DONE is a perverse incentive.** Models are trained to conclude conversations. Emitting DONE is the path of least resistance to end the session. Self-reported completion is inherently unreliable.
4. **No legitimate path for no-op tasks.** Tasks already implemented by another branch, or investigation-only tasks, have no way to signal "no changes needed" — they must lie and say DONE.

### Reference Systems Studied

**ralph-claude-code** uses git diff as ground truth. After each loop iteration, it checks committed + uncommitted changes. A circuit breaker halts execution after N consecutive loops with no file changes. No single marker is authoritative — multiple signals (completion keywords, file changes, output length trends, explicit exit signal) feed a confidence score. Permission denial detection catches blocked tools.

**gastown** uses structural role separation (delegate mode — coordinators literally cannot edit files). Workers (polecats) self-manage completion via `gt done` which pushes the branch and goes idle. The witness is a safety net for anomalies, not a gatekeeper for every completion. Escalation protocol routes stuck work through tiers (agent → deacon → mayor). Crash loop prevention: 3 crashes on the same step = escalate, stop retrying. "Discover, don't track" — agents check observable state (git, beads), not self-reports.

---

## Decision

### 1. Workers: Outcome-Based Validation (Git Diff as Ground Truth)

**Drop `WORKER_RESULT: DONE` as the primary routing signal.** The model stopping tool calls is the natural completion signal (that's how every agent loop works). After the reply loop ends:

1. Run `git diff --stat` in the worktree (both staged and unstaged).
2. **Diff is non-empty** → worker produced changes → proceed to reviewer. No marker needed.
3. **Diff is empty + worker never used write/edit tools** → evidence-based nudge (see §3).
4. **Diff is empty + worker explicitly signaled `NO_CHANGES_NEEDED <reason>`** → pass to reviewer with the reason. Reviewer verifies the claim.

New marker: `WORKER_RESULT: NO_CHANGES_NEEDED <reason>` — a high-friction exit for legitimate no-ops (task already implemented by another branch, investigation-only, etc.). The worker must justify itself. The reviewer then independently verifies the claim.

`WORKER_RESULT: DONE` is kept for backwards compatibility and logging but is no longer the routing decision. Git diff is.

### 2. Reviewers: AC-Driven Verdicts (Structural, Not Self-Reported)

**Replace text marker routing with acceptance criteria state.**

- Reviewer MUST call `task_update(id, acceptance_criteria=[...])` with each criterion marked `met: true` or `met: false`. This is already supported by the `AcceptanceCriterionStatus` type.
- **All ACs met** → VERIFIED. Derived from AC state, not from a text marker.
- **Any AC not met** → REOPEN. Feedback is the unmet criteria descriptions.
- **Reviewer doesn't update ACs** → default to REOPEN (conservative). No nudge needed.

**Workers no longer update AC status.** Only reviewers do. This prevents workers from gaming the system by marking their own work as complete.

Text markers (`REVIEW_RESULT: VERIFIED/REOPEN`) are kept for logging and backward compatibility, but the AC state is the authoritative routing signal.

### 3. Evidence-Based Nudging

**Replace the current vague nudge with concrete evidence.**

Current nudge (broken): "Emit exactly one final marker now: WORKER_RESULT: DONE" — tells the model to quit.

New nudge (when git diff is empty and no `NO_CHANGES_NEEDED` signal):

> "Your session produced zero file changes in the worktree. `git diff --stat` output: (nothing). The task requires code modifications. Use the developer write/edit tools to implement the changes described in the acceptance criteria. If this task is genuinely already done or requires no code changes, signal `WORKER_RESULT: NO_CHANGES_NEEDED` with a reason explaining why."

This is:
- **Evidence-based** — shows the model objective proof it hasn't done anything
- **Actionable** — tells it exactly what to do (use write/edit tools)
- **Has an escape hatch** — NO_CHANGES_NEEDED for legitimate cases

**Max 2 nudge attempts.** After the second nudge with still no file changes, fail the task with reason "worker produced no changes after 2 attempts."

### 4. Circuit Breaker for Task-Level Failure

Track consecutive failed attempts per task:

| Condition | Action |
|-----------|--------|
| Worker produces no changes after 2 nudges | Mark task `failed`, reason: "no file changes produced" |
| Task reopened 3+ times by reviewer | Mark task `failed`, reason: "exceeded reopen limit" |
| Worker session errors 3+ times consecutively | Mark task `failed`, reason: "consecutive session failures" |

All conditions transition the task to `pm_review` state, triggering PM agent dispatch (see §5). Zero human triage — the PM agent resolves autonomously.

### 5. PM/Planner Agent (Escalation Target — Zero Human Triage)

**When the circuit breaker trips, the PM agent is dispatched — not a human.** Failed tasks must not pile up for human triage. The PM is the autonomous escalation tier (gastown's Deacon equivalent).

**Trigger:** Any circuit breaker condition from §4 transitions the task to `pm_review` state, which triggers PM agent dispatch.

**New task state:** `failed` is replaced by `pm_review`. The state machine gains:
- `in_progress → pm_review` (circuit breaker: no changes after nudges)
- `needs_task_review → pm_review` (circuit breaker: reopen limit exceeded)
- `open → pm_review` (circuit breaker: consecutive session failures)

**PM agent capabilities:**
- Tools: `task_create`, `task_update`, `task_list`, `task_show`, `memory_search`, `memory_read`, `shell` (read-only, no worktree). NO write/edit — planning, not coding.
- Can read the codebase via shell to understand scope and complexity.
- Can read task activity log (reviewer feedback, session errors) to understand why the worker failed.

**PM agent actions:**

| Situation | Action |
|-----------|--------|
| Task too large / complex | **Decompose**: create subtasks with blockers, transition parent to `decomposed` |
| Task poorly scoped / ambiguous | **Rescope**: rewrite description, design, and/or AC, transition back to `open` for re-dispatch |
| Missing dependency not on board | **Create dependency**: create the missing task, add as blocker, transition parent back to `open` |
| External blocker (needs human input) | **Mark blocked**: transition to `blocked` with reason — this is the ONLY path to human attention |

**New task states:**
- `pm_review` — PM agent is evaluating the failed task
- `decomposed` — parent task was split into subtasks (terminal state, like `closed`)

**Subtask dispatch:** New subtasks created by PM get picked up immediately by workers. Blockers handle sequencing. Parent task's worktree is cleaned up before PM runs (PM has no worktree).

**PM failure:** If the PM agent itself fails (session error, can't determine action), the task transitions to `blocked` with reason "PM agent could not resolve — needs human review." This should be rare since PM work is lightweight (reading + planning, not coding).

## Consequences

### What Changes

| Component | Current | New |
|-----------|---------|-----|
| Worker completion signal | `WORKER_RESULT: DONE` text marker | `git diff --stat` in worktree |
| Worker no-op path | Not supported | `NO_CHANGES_NEEDED <reason>` → reviewer verifies |
| Reviewer verdict | `REVIEW_RESULT: VERIFIED/REOPEN` text | AC `met` state on each criterion |
| Nudge trigger | Missing text marker | Empty git diff |
| Nudge content | "Emit your marker" | Evidence: "git diff is empty, use write/edit tools" |
| Nudge limit | Unlimited (1 attempt, then error) | Max 2 attempts, then fail task |
| Task failure | Only on session error | Also on: no changes, reopen limit, session error limit |
| AC updates by worker | Allowed | Removed — only reviewers update AC |

### Files Affected

- `src/actors/slot/lifecycle.rs` — post-loop: add git diff check, replace marker-based routing
- `src/actors/slot/reply_loop.rs` — track write-tool usage, simplify nudge logic
- `src/agent/output_parser.rs` — add `NoChangesNeeded` variant to `WorkerSignal`
- `src/actors/slot/epic_review.rs` — `success_transition()`: derive reviewer verdict from AC state
- `src/agent/prompts/dev.md` — remove DONE marker requirement, add NO_CHANGES_NEEDED docs
- `src/agent/prompts/task-reviewer.md` — make AC update mandatory, remove REVIEW_RESULT as routing signal
- `src/mcp/tools/task_tools/` — restrict AC updates: add `reviewer_only` guard or similar
- `src/actors/slot/helpers.rs` — add `git_diff_stat()` helper for worktree

### What Stays the Same

- Epic reviewer markers (`EPIC_REVIEW_RESULT: CLEAN/ISSUES_FOUND`) — epic review has no git diff equivalent
- Compaction detection and threshold logic
- Session cost tracking and token metrics
- Goose reply loop mechanics
- Worktree lifecycle (create, commit, cleanup)
- Verification commands (setup + verify still run before reviewer)

### Risks

1. **Git diff false positives** — worker writes files then reverts. Mitigated: if diff is empty after write-tool usage, treat as suspicious and nudge.
2. **Reviewer doesn't update ACs** — defaults to REOPEN, which is safe but could cause unnecessary rework. Mitigated: prompt is explicit and mandatory.
3. **NO_CHANGES_NEEDED abuse** — worker uses it to skip work. Mitigated: reviewer independently verifies the claim; if false, REOPEN.
4. **AC state mutation by worker** — worker could call task_update before restriction is enforced. Mitigated: restrict at the tool level, not just prompt level.

---

## Relations

- [[ADR-012 Epic Review Batches and Structured Output Nudging]] — this ADR supersedes the worker/reviewer nudging aspects
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — execution model unchanged
- [[V1 Requirements]] — extends REVIEW-01, REVIEW-03, AGENT-08 (stuck detection)
- [[Roadmap]] — adds Phase 10
