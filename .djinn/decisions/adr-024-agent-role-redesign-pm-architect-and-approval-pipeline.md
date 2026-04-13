---
title: ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline
type: adr
tags: ["adr","agents","pm","architect","approval"]
---


# ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline

**Status:** Proposed
**Date:** 2026-03-06
**Supersedes:** Partially supersedes ADR-012 (epic review batches), ADR-016 (epic reviewer worktree)
**Related:** [[decisions/adr-022-outcome-based-session-validation-agent-role-redesign|ADR-022: Outcome-Based Session Validation & Agent Role Redesign]], [[decisions/adr-023-cognitive-memory-architecture-multi-signal-retrieval-and-associative-learning|ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]

---

## Context

The current agent model has four types: Worker, TaskReviewer, EpicReviewer, ConflictResolver. Problems:

1. **EpicReviewer is reactive and low-value.** It runs after tasks close, reviews aggregate diffs, and creates follow-up tasks. In practice, it duplicates work the TaskReviewer already did and produces vague "pattern improvement" suggestions without codebase-wide context.

2. **No autonomous task grooming.** Tasks go directly from `open` to dispatch. There's no quality gate ensuring tasks are well-scoped, have proper AC, reference correct ADRs, and are implementable without ambiguity. Workers frequently fail because tasks are underspecified.

3. **No circuit breaker escalation target.** ADR-022 introduced circuit breakers (no changes after nudges, reopen limit, session errors) but the escalation target was underspecified. Failed tasks need an agent that can decompose, rescope, or create missing dependencies — not just flag for human triage.

4. **Workers have too many tools.** Workers currently have `task_update` which lets them modify task descriptions, AC, design, labels — but a worker's job is to write code, not redefine the task. This creates a loophole where workers can mark their own ACs as met (ADR-022 §2 identified this).

5. **No proactive codebase analysis.** Nobody checks whether implementation matches ADRs, whether patterns are being followed, whether technical debt is accumulating. This is discovered only when things break.

6. **KB hygiene is nobody's job.** Notes become stale, ADRs get superseded without updates, wikilinks break, orphan notes accumulate. ADR-023's cognitive memory features (confidence scoring, contradiction detection) need an agent that acts on these signals.

### Reference: Existing Agent Prompts

Existing agent prompts at `/home/fernando/git/djinnos/agents/` informed this design:
- **PM (Paul)**: Synthesizes research into epics/stories, creates roadmaps, manages priorities
- **Architect (Archie)**: System design, ADRs, challenges over/under-engineering, finds libraries
- **Worker (Dave)**: Implements tasks with TDD, strict scope discipline, scoped commits
- **Task Reviewer (Sam/SM)**: Validates completion quality, enforces AC, outcome-driven

## Decision

### 1. Kill EpicReviewer

Remove `AgentType::EpicReviewer` entirely. Delete:
- Epic review batch orchestration (ADR-012)
- `EPIC_REVIEW_RESULT` marker parsing
- `epic_review.rs` in slot module
- Epic review prompt template
- `EpicReviewer` variant from `AgentType` enum
- Detached HEAD worktree logic (ADR-016)

Epic-level quality is replaced by: (a) Architect proactive codebase analysis, and (b) PM grooming ensuring tasks are well-defined before dispatch.

### 2. New Agent Type: PM

**Purpose:** Backlog grooming, circuit breaker escalation, KB hygiene (structural), roadmap/scope maintenance.

**Tools:**
- `task_create`, `task_update`, `task_list`, `task_show`, `task_transition` (backlog→open, failed→blocked/decomposed/open)
- `task_comment_add`, `task_activity_list`
- `memory_read`, `memory_search`, `memory_write`, `memory_edit`, `memory_catalog`, `memory_health`, `memory_orphans`, `memory_broken_links`
- `shell` (read-only, no worktree)

**KB catalog injection:** The PM's kickoff message includes the full `memory_catalog` output plus KB health metrics (orphan count, broken links, stale notes). This gives the PM immediate awareness of the KB state.

**PM does NOT get:**
- `task_transition` with `accept` action for ADRs — only humans approve Proposed ADRs
- Write/edit tools for code — PM plans, doesn't code
- `memory_delete` — PM can flag notes but not destroy them (only humans delete)

**Prompt philosophy (from existing PM prompt):**
- "Synthesize, don't duplicate" — aggregate existing context
- Tasks must be dev-ready: explicit scope (IN/OUT), step-by-step approach, ADR references, testable AC
- The "Lazy Dev Philosophy" — tasks so complete that a worker can implement without making any decisions

### 3. New Agent Type: Architect

**Purpose:** Proactive codebase analysis, ADR enforcement, pattern verification, ADR proposals, KB hygiene (semantic).

**Tools:**
- `task_create` (enforcement tasks referencing Accepted ADRs)
- `task_show`, `task_list`
- `task_comment_add`
- `memory_read`, `memory_search`, `memory_write`, `memory_edit`, `memory_catalog`, `memory_health`
- `shell` (read-only, no worktree)

**Architect does NOT get:**
- `task_transition` — architect creates tasks in backlog, PM grooms them
- `task_update` — architect doesn't modify existing tasks
- Code write/edit tools — architect analyzes, doesn't implement

**Two modes of operation:**

**Enforcement (autonomous):** When an existing Accepted ADR is violated by code, the architect creates a task in backlog referencing the ADR. No approval needed — the decision was already made and approved.

**Proposals (needs human approval):** When the architect identifies a new architectural concern (library upgrade, pattern change, refactoring opportunity), it writes a **Proposed ADR** via `memory_write(type="adr")`. It does NOT create tasks for proposals. The ADR sits in the approval queue until a human acts.

### 4. ADR Status as System Semantics

ADR status gains operational meaning beyond documentation:

| Status | Meaning | Who sets it | Effect |
|--------|---------|-------------|--------|
| `Proposed` | Architect's recommendation, awaiting human approval | Architect | Not enforceable. PM will not approve tasks referencing Proposed ADRs. |
| `Accepted` | Human-approved decision | Human (via desktop/MCP) | Enforceable. Tasks can reference it and flow through grooming. |
| `Superseded` | Replaced by a newer ADR | PM, Architect, or Human | Tasks referencing it should be updated. |
| `Rejected` | Human declined the proposal | Human | Archived. No tasks created. |

**Approval pipeline:**
1. Architect writes Proposed ADR
2. Desktop surfaces it in approval queue (query: `memory_search(type="adr", query="Proposed")`)
3. Human reviews → Approve or Reject
4. On approval: ADR status → Accepted, PM dispatched to plan and create tasks from it (like `/plan` → `/breakdown` pipeline)
5. On rejection: ADR status → Rejected, archived

**MCP restriction:** The `memory_edit` tool for changing ADR status from Proposed→Accepted is restricted to human callers (not agent sessions). Agents can write Proposed ADRs and change Accepted→Superseded, but cannot self-approve.

### 5. Worker Tool Restriction

Workers lose `task_update` entirely. New worker tool set:

| Tool | Purpose |
|------|---------|
| `task_show` | Read task details |
| `task_comment_add` | Leave comments (progress, blockers, discoveries) |
| `memory_read` | Read KB notes referenced by task |
| `memory_search` | Search KB for patterns, ADRs |
| `shell` | Execute commands in worktree (sandboxed) |

Workers communicate through comments. If a task is underspecified, the worker leaves a comment explaining what's unclear. The circuit breaker (ADR-022) eventually routes the task to PM review if the worker can't produce changes.

### 6. TaskReviewer Tool Adjustment

TaskReviewer keeps `task_update` but ONLY for updating acceptance criteria `met` status (ADR-022 §2). The reviewer also keeps `shell` for running verification commands.

| Tool | Purpose |
|------|---------|
| `task_show` | Read task details |
| `task_update` | Update AC met/unmet status only |
| `task_comment_add` | Leave review feedback |
| `memory_read` | Read referenced KB notes |
| `memory_search` | Search KB |
| `shell` | Run verification commands in worktree |

### 7. KB Hygiene Responsibilities

Split between PM and Architect based on the nature of the work:

**PM (structural hygiene, runs during grooming):**
- Fix broken wikilinks
- Remove/archive orphan notes
- Update roadmap progress
- Update scope notes
- Create tasks for deeper KB issues it can't fix inline

**Architect (semantic hygiene, runs during codebase analysis):**
- Cross-reference ADRs against code reality
- Update/supersede ADRs where implementation diverged
- Update pattern notes
- Flag contradictions discovered during analysis

**Automated (no agent, runs post-task-close):**
- Update `last_accessed` on read notes
- Compute co-access associations (ADR-023 §2)
- Update confidence scores (ADR-023 §3)
- Flag referenced KB notes for freshness review

### 8. PM Intervention Examples (from Forge project, 2026-03-09)

These real cases from the Forge project demonstrate when and how the PM should intervene. All four tasks ran without a PM, exposing the gaps this ADR addresses.

#### Case A: Worker refuses to scaffold new project types (ofeh, vz71)

**Tasks:** `ofeh` (SvelteKit scaffold + Axum backend + rust-embed) and `vz71` (Go CLI tool for RabbitMQ event type extraction).

**What happened:** Both tasks required creating entirely new project types (npm/SvelteKit, Go module) inside an existing Rust workspace. The worker agent inspected the worktree, saw only Rust code, and repeatedly concluded it was "blocked" — responding with text-only summaries instead of writing code. After 50+ reopens and 100+ sessions, zero commits were produced.

**Why:** The worker was dispatched directly to an ambitious, multi-technology task. No grooming step verified that the task was implementable by the assigned model. The worker prompt said "implement" but the model (Codex) doesn't confidently scaffold unfamiliar ecosystems.

**PM intervention — decompose:**
1. Detect the pattern: high reopen count (>5), zero diff, worker comments saying "workspace missing X"
2. Read the worker's comments to understand what's missing
3. Decompose the task. For `ofeh`, split into:
   - "Create dashboard/ directory with SvelteKit + adapter-static + Tailwind" (explicit file-by-file scaffold)
   - "Add Axum SSE endpoint for container health events"
   - "Integrate rust-embed to serve dashboard SPA from binary"
4. Each sub-task references only one technology and has concrete file paths in its design
5. Transition original task to `blocked` with sub-tasks as blockers

**PM intervention — rescope AC:**
For `vz71` (Go tool), the PM might also recognize this task belongs in a separate Go module repo, not the Rust workspace, and update the task design accordingly.

#### Case B: Worker-reviewer bounce on unverifiable ACs (gp81, tlkt)

**Tasks:** `gp81` (forge db reset — drop/recreate DB, re-run migrations) and `tlkt` (GripMock gRPC mock containers with DNS aliases).

**What happened:** Workers made real progress — `gp81` had 154 insertions with 3/5 ACs met, `tlkt` had 123 insertions with 4/5 ACs met. But specific ACs kept failing review:
- `gp81`: "Migrations re-run after database recreation" and "--seed default loads seed after reset" — require a running Postgres container
- `tlkt`: "Proto file loaded and gRPC server responds to defined methods" — requires a running GripMock container

The reviewer correctly marked these unmet (no Docker in the worktree to verify), the task was rejected, the worker tried again, and the cycle repeated 10-13 times.

**Why:** The ACs mix code-verifiable criteria (struct exists, function compiles) with runtime-verifiable criteria (container responds on port). The reviewer can only inspect code and run build commands — it cannot start Docker containers.

**PM intervention — adjust ACs:**
1. Detect the pattern: high reopen count (>5), partial AC progress (some met, same ones always unmet), worker diffs are stable (no new changes between cycles)
2. Read the reviewer's rejection comments and the worker's code
3. Split ACs into **code-verifiable** and **integration-verifiable**:
   - Code-verifiable: "Function exists that generates the correct psql DROP/CREATE commands" → reviewable
   - Integration-verifiable: "Migrations re-run after database recreation" → flag as `integration_test`, not blockable by task reviewer
4. Update the task's ACs via `task_update` so the reviewer only gates on what it can actually verify
5. Create a separate integration test task that runs after merge with real Docker

**General principle:** The PM should intervene whenever the worker-reviewer cycle exceeds a threshold (proposed: 5 reopens) without the task closing. The PM reads all comments, inspects the diff, and either decomposes, rescopes ACs, changes the task design, or escalates to the human.

## Consequences

### What Changes

| Component | Current | New |
|-----------|---------|-----|
| Agent types | Worker, TaskReviewer, EpicReviewer, ConflictResolver | Worker, TaskReviewer, PM, Architect, ConflictResolver |
| Worker tools | task_show, task_update, task_comment_add, memory_read, memory_search, shell | task_show, task_comment_add, memory_read, memory_search, shell |
| Epic review | Batch orchestration per ADR-012 | Eliminated. Replaced by Architect codebase analysis. |
| Task grooming | None (direct open→dispatch) | PM reviews backlog before tasks become open |
| Circuit breaker target | Underspecified "pm_review" | PM agent dispatched with full context |
| ADR status | Documentation-only | Operational: Proposed/Accepted/Superseded/Rejected with system semantics |
| KB maintenance | Nobody | PM (structural) + Architect (semantic) + automated (post-task) |
| Codebase analysis | Only via epic review diffs | Architect proactive analysis triggered by codebase change threshold |

### Files Affected

- `src/agent/mod.rs` — add `PM`, `Architect` to `AgentType`, remove `EpicReviewer`
- `src/agent/extension.rs` — rewrite `config()` with new tool sets per agent type
- `src/agent/prompts/` — add `pm.md`, `architect.md`, remove `epic-reviewer-batch.md`
- `src/agent/output_parser.rs` — remove `EPIC_REVIEW_RESULT` parsing
- `src/actors/slot/epic_review.rs` — delete entirely
- `src/actors/slot/lifecycle.rs` — add PM/Architect dispatch paths
- `src/actors/coordinator/dispatch.rs` — add PM/Architect trigger logic
- `src/mcp/tools/memory_tools/` — add status restriction for ADR approval
- `src/db/repositories/epic_review_batch.rs` — delete
- `src/models/epic_review_batch.rs` — delete

### What Stays the Same

- Worker coding workflow (Goose sessions, worktrees, sandboxed shell)
- TaskReviewer code review flow (AC verification, git diff)
- ConflictResolver (merge conflict resolution)
- Session cost tracking (ADR-010)
- Goose library as agent harness (ADR-008)
- OS-level shell sandboxing (ADR-013)

### Risks

1. **PM as bottleneck** — all tasks flow through PM grooming. Mitigated: debounced trigger, batch processing, re-spawn while backlog > 0.
2. **Architect creating too many tasks** — proactive analysis could flood the backlog. Mitigated: PM grooming filters low-value tasks; architect prompt should prioritize high-impact findings.
3. **ADR approval queue ignored by human** — Proposed ADRs pile up. Mitigated: desktop notifications; architect only proposes when there's a clear need.
4. **Prompt-only enforcement** — tool restrictions for workers are structural, but PM/Architect behavior boundaries are prompt-based. Mitigated: PM gating provides a second layer (won't approve tasks with Proposed ADR refs).

---

## Relations

- [[decisions/adr-022-outcome-based-session-validation-agent-role-redesign|ADR-022: Outcome-Based Session Validation & Agent Role Redesign]] — circuit breaker escalation target
- [[decisions/adr-023-cognitive-memory-architecture-multi-signal-retrieval-and-associative-learning|ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]] — KB hygiene signals
- [[decisions/adr-012-epic-review-batches-and-structured-output-nudging|ADR-012 Epic Review Batches and Structured Output Nudging]] — superseded (epic review eliminated)
- [[decisions/adr-016-epicreviewer-detached-head-worktree|ADR-016: EpicReviewer Detached HEAD Worktree]] — superseded (epic reviewer eliminated)
- [[Roadmap]] — adds to Phase 10