---
title: ADR-034: Agent Role Hierarchy — Architect Patrol, Scrum Master Rules, and Task Types
type: adr
tags: ["adr","architecture","agents","roles","architect","scrum-master","dispatch","escalation","task-types"]
---

# ADR-034: Agent Role Hierarchy — Architect Patrol, Task Types, and Escalation

## Status: Accepted
Draft

Date: 2026-03-16

Supersedes: ADR-024 (Agent Role Redesign — PM, Architect, and Approval Pipeline)

## Context

### The ADR-029 Failure

The Vertical Workspace Split (ADR-029) produced 88 tasks across 2 epics, most force-closed. Root cause: no agent validated the *approach* before work started. The PM decomposed blindly, workers executed blindly, and when tasks kept failing nobody stepped back to ask "is the approach wrong?"

### Current Role Gaps

The current agent hierarchy is flat:

```
PM (on-demand, worker escalation) → intervenes on individual tasks
Worker → executes tasks
Groomer (backlog trigger) → validates task quality
TaskReviewer → reviews completed work
ConflictResolver → handles merge conflicts
```

**Missing capabilities:**
1. **No proactive health monitoring.** Nobody watches epic-level patterns (force-close rates, throughput drops, stuck tasks).
2. **No strategic re-planning.** When the PM keeps force-closing tasks, nobody asks why.
3. **No spike/research mechanism.** Complex work goes straight to workers without feasibility validation.
4. **No escalation ceiling above PM.** Workers escalate to PM, PM has nowhere to go.
5. **No session timeouts.** PM/Groomer sessions can run indefinitely.
6. **Non-code agents (Groomer, PM) are invisible.** They run as project-scoped sessions with no kanban visibility.

### Gastown Reference

The Gastown project (reference implementation) uses a Deacon daemon that runs Doctor Dog health checks every 5 minutes, with tiered escalation to Mayor for strategic decisions. Key insight: cheap periodic patrol catches systemic issues before they compound.

## Decision

### 1. Role Renames

| Old Name | New Name | Rationale |
|---|---|---|
| Groomer | **Planner** | Plans, decomposes epics, creates spikes |
| PM | **Lead** | Tech lead — unblocks workers, triages stuck tasks |
| (new) | **Architect** | Strategic review, health patrol, approach validation |
| TaskReviewer | **Reviewer** | Reviews completed work (unchanged behavior) |
| ConflictResolver | **Resolver** | Merge conflicts (unchanged behavior) |
| Worker | **Worker** | No change |

### 2. Humans Create Epics Only — No Backlog State

Humans never create tasks directly. They describe ideas via chat, and the chat agent creates epics. The Planner handles all task creation downstream.

**Flow:**
```
Human (chat) → describes idea/goal
    ↓
Chat agent → asks clarifying questions → creates epic with description + memory_refs
    ↓
Planner dispatched (epic_created trigger) → creates spikes or first batch of tasks
    ↓
Workers/Architect execute
```

**Backlog state is removed.** All tasks are created by agents (Planner, Architect, Lead) who validate quality at creation time. There is no unvalidated intake queue.

**Simplified task states:**
- Worker tasks: `open → in_progress → verifying → needs_review → in_review → closed`
- Non-worker tasks: `open → in_progress → closed`

### 3. New Role: Architect (5-minute patrol + on-demand escalation)

The Architect is a **project-scoped agent dispatched every 5 minutes** to review board health. Also dispatched on-demand via Lead escalation or `request_architect` tool.

**Dispatch triggers:**
1. **5-minute patrol** — coordinator dispatches if no Architect session running. Creates a `review` task for visibility.
2. **Lead escalation** — Lead calls `request_architect` when task-level intervention isn't enough.
3. **2nd request_lead on same task** — coordinator routes directly to Architect instead of Lead.

**What it checks:**
- Board-wide throughput: tasks merged recently, tasks stuck, tasks in Lead intervention
- Epic health: force-close rates, reopen rates, progress vs stall
- Worker sessions: running too long with no output, crash loops
- Lead sessions: stuck or timing out
- Approach viability: are tasks in an epic achievable given codebase state?

**What it can do:**
- Kill stuck worker/Lead sessions
- Force-close bad tasks in bulk
- Create spike/research tasks for complex work
- Write ADRs and research notes to memory
- Restructure epics (update description/strategy, close broken epics)
- Create new tasks with appropriate task types
- Read full codebase (shell, read, grep, lsp — no write/edit)

**What it cannot do:**
- Edit code (no write/edit/apply_patch)
- Merge branches

**Session timeout:** 10 minutes.

### 4. Scrum Master as Coordinator Rules (no LLM)

The Scrum Master is NOT an LLM agent — it is **deterministic coordinator logic** in the existing tick loop. Zero LLM cost.

**Rules:**
- Non-worker session (Lead, Planner, Architect) running > 10 minutes → kill session
- 2nd `request_lead` escalation on same task → route to Architect instead of Lead
- Spike/research task completed under an epic → create `decomposition` task for Planner (next batch)
- Track throughput metrics per epic (tasks merged per hour, rolling window)

### 5. Expanded Task Types with Default Workflows

Extend `issue_type` to determine default agent, lifecycle, and verification:

| issue_type | Default agent | Lifecycle | Verification | Review |
|---|---|---|---|---|
| `task` | Worker | Full | Yes | Yes |
| `feature` | Worker | Full | Yes | Yes |
| `bug` | Worker | Full | Yes | Yes |
| `spike` | Architect | Simple | No | No |
| `research` | Architect | Simple | No | No |
| `decomposition` | Planner | Simple | No | No |
| `review` | Architect | Simple | No | No |

**Simple lifecycle:** `open → in_progress → closed`. Skips verifying, needs_review, in_review.

**Agent type override:** Optional `agent_type` field on tasks overrides the default from `issue_type`.

### 6. Planner as Roadmap Maintainer — Wave-Based Decomposition

The Planner owns the **per-epic roadmap** — a living design note that evolves across batches. It is the Planner's working memory across decomposition cycles.

**Trigger:** Epic created → coordinator creates `decomposition` task → dispatches Planner.

**First decomposition (epic just created):**
1. Planner reads epic description, ADRs, memory refs
2. Calls `build_context(intent="epic:{epic_id}")` to load relevant knowledge
3. Reads the codebase to understand complexity
4. Writes a `design` note: "Roadmap: {epic_title}" linked to the epic via `memory_refs`
   - Overall approach and rationale
   - Phases/batches planned (high-level)
   - Risks and unknowns identified
5. Decides: spike first, or straight to worker tasks?
6. Creates spike tasks (for Architect) OR first batch of 3-5 worker tasks
7. **Never creates more than 5 tasks at once**

**Subsequent decompositions (batch completed):**

When the Scrum Master rules detect all tasks in the current batch are closed and the epic is still open, a new `decomposition` task is created and the Planner is re-dispatched.

1. Planner reads the epic's roadmap note
2. Calls `build_context` — now enriched with:
   - **Session reflection notes** (ADR-023 §7): cases, patterns, pitfalls extracted from completed worker sessions
   - **Confidence changes** (ADR-023 §4): referenced ADRs/notes may have gained or lost confidence based on task outcomes
   - **Implicit associations** (ADR-023 §3): new knowledge connections emerged from worker co-access patterns
3. Updates the roadmap note:
   - "Batch N results: X/Y tasks merged, Z failed because..."
   - "Learnings: approach A worked well, approach B didn't because..."
   - "Adjusted plan: switching from X to Y for next batch"
   - "Batch N+1 plan: ..."
4. Creates next batch of 3-5 worker tasks
5. If Planner determines the epic's goal is met → closes the epic

**Scrum Master rule for batch completion:**
```
All non-decomposition/review tasks under epic are closed
  AND epic is still open
  → create decomposition task for Planner
```

**Integration with Cognitive Memory (ADR-023):**

The wave/batch flow creates a natural feedback loop with the cognitive memory system:

| Memory Signal | Source | How Planner Uses It |
|---|---|---|
| Session reflection (cases, pitfalls) | ADR-023 §7 — extracted after each worker session | Informs next batch design, avoids repeated mistakes |
| Confidence scoring | ADR-023 §4 — task success/failure updates note confidence | Low confidence on epic's ADR = approach may be wrong |
| Implicit associations | ADR-023 §3 — worker co-access patterns | Surfaces relevant knowledge Planner didn't explicitly link |
| Contradiction detection | ADR-023 §5 — conflicting notes flagged on write | Catches when worker findings contradict the roadmap |

The Architect patrol also reads the roadmap note during health reviews. If the Architect determines the approach is fundamentally wrong, it updates the roadmap with its analysis and creates a new spike task, restarting the wave cycle with corrected strategy.

### 7. Self-Task Creation for Non-Worker Agents

All non-worker agents create a visible task before running:
- **Architect patrol:** `review` task — "Board health review"
- **Architect escalation:** `review` task — "Review: {epic_title}" or "Investigate: {task_title}"
- **Planner decomposition:** `decomposition` task — "Decompose: {epic_title}"

These show up on the kanban board. Full session history, Langfuse visibility.

### 8. Escalation Ladder

```
Worker fails → reopen (automatic, up to 2 times)
3rd failure → Lead intervention (10min timeout)
Lead can't resolve → Architect picks up on next patrol
2nd request_lead on same task → skip Lead, route to Architect
Lead calls request_architect → Architect dispatched immediately
Architect patrol (every 5min) → catches everything else
```

The Architect is the escalation ceiling. If it can't resolve, it leaves a comment and the task stays for human review.

### 9. Session Timeouts

| Agent | Timeout |
|---|---|
| Worker | No hard timeout (verification gate is backstop) |
| Lead | 10 minutes |
| Planner | 10 minutes |
| Architect | 10 minutes |
| Reviewer | 10 minutes |
| Resolver | 10 minutes |

Non-worker sessions exceeding timeout are killed by coordinator (Scrum Master rules).

## Consequences

### Positive
- Systemic failures caught within 5 minutes instead of compounding for hours
- Complex work validated via spikes before worker decomposition
- All agent work visible on kanban board (no invisible sessions)
- Lead escalation has a ceiling (Architect)
- Session timeouts prevent runaway LLM costs
- Incremental decomposition (5 tasks at a time) prevents 88-task explosions
- Scrum Master rules are deterministic — zero LLM cost for mechanical decisions
- Task types make the board self-documenting (spikes, reviews, decompositions visible)
- No backlog state — simpler lifecycle, no unvalidated task queue
- Humans only create epics via chat — cleaner separation of concerns

### Negative
- Architect patrol every 5 minutes adds LLM cost even when nothing is wrong
- More task types add complexity to coordinator dispatch logic
- Self-task creation means more tasks on the board (noise vs signal)
- Removing backlog requires migrating existing backlog tasks

### Mitigations
- Architect patrol skips if no open epics or no active work (cheap pre-check)
- Architect uses fast/cheap model for health checks, powerful model only for deep analysis
- `review` tasks auto-close as `completed` if Architect finds nothing wrong (minimal noise)

## Migration

1. Rename `AgentType::Groomer` → `Planner`, `AgentType::Pm` → `Lead`, `TaskReviewer` → `Reviewer`, `ConflictResolver` → `Resolver`
2. Remove `backlog` from TaskStatus enum and state machine; migrate existing backlog tasks to `open`
3. Add `spike`, `research`, `decomposition`, `review` to `issue_type` enum
4. Add optional `agent_type` field to tasks table (nullable, defaults based on issue_type)
5. Implement Architect role (RoleConfig, prompt, tool schema, simple lifecycle)
6. Implement simple lifecycle path for non-worker task types (open → in_progress → closed)
7. Add epic_created trigger → Planner dispatch with `decomposition` task creation
8. Add Scrum Master rules to coordinator tick (10-min timeout, 2nd-escalation routing, throughput tracking)
9. Add `request_architect` tool for Lead
10. Update Planner prompt: create spikes for complex work, max 5 tasks per batch
11. Update Lead prompt: 10-min awareness, `request_architect` escalation, clear notes for Architect

## Relations

- [[ADR-024: Agent Role Redesign]] — superseded
- [[ADR-025: Backlog Grooming and Autonomous Dispatch Triggers]] — extended (Groomer gets epic trigger)
- [[ADR-022: Outcome-Based Session Validation and Agent Role Redesign]] — complementary
- [[ADR-033: Incremental Crate Extraction]] — motivated this ADR (past failure analysis)
- [[ADR-023: Cognitive Memory Architecture]] — wave/batch flow integrates with session reflection (§7), confidence scoring (§4), implicit associations (§3), and contradiction detection (§5)