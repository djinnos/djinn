---
title: "ADR-051: Planner-as-Patrol, Architect-as-Consultant, ADR-to-Epic Pipeline, and Auto-Dispatch Reentrance Guards"
type: adr
tags: ["adr","architecture","agents","roles","planner","architect","patrol","auto-dispatch","reentrance","adr-pipeline","pulse"]
---



# ADR-051: Planner-as-Patrol, Architect-as-Consultant, ADR-to-Epic Pipeline, and Auto-Dispatch Reentrance Guards

## Status: Draft

Date: 2026-04-08

Supersedes (partially): [[ADR-034 Agent Role Hierarchy — Architect Patrol, Task Types, and Escalation]]
Extends: [["ADR-050: Architect/Chat Code-Graph Consolidation, Canonical SCIP Indexing, and Graph Query Extensions"]], [[ADR-046 Chat-Driven Planning]]
Related: [[ADR-025 Backlog Grooming and Autonomous Dispatch Triggers]], [[ADR-023 Cognitive Memory Architecture]], [[ADR-043 Repository Map — SCIP-Powered Structural Context]], [[ADR-047 Repo-Graph Query Seam]]

## Context

### The two-jobs problem

ADR-034 made the Architect responsible for two very different kinds of work:

1. **Board janitor** — 5-minute patrol, dedupe tasks, force-close stuck work, spawn planners, keep the kanban tidy, serve as the escalation ceiling above Lead.
2. **Code reasoning** — warm the canonical SCIP graph, detect cycles/orphans/hotspots, write ADRs, flag approach viability, catch structural drift.

These have opposite cadences, opposite costs, and opposite risk profiles. Janitor work should be frequent, cheap, and idempotent. Code reasoning should be infrequent, expensive, and produce durable artifacts. Collapsing them into one role means the cheap thing fires constantly and the expensive thing rarely fires at all — which is exactly what today's behaviour shows.

### Two operational bugs confirm the coupling is broken in practice

Empirical evidence from 2026-04-08:

- **Agent bridge is missing `code_graph`.** Architect sessions log `code_graph not available in agent bridge — use MCP server`. The architect runs, but its bridge-side tool surface does not include `code_graph`, so every patrol silently fails the code-reasoning half of its job. Only the janitor half runs. This is a pre-existing bug visible to anyone checking architect traces in Langfuse — ADR-050 assumes `code_graph` reaches the architect; the bridge wiring never landed.
- **Read ops hang despite `warmed: true`.** Calling `code_graph status` returns `warmed: true, last_warm_at: 2026-04-08T12:37:37Z, pinned_commit: 5dac07c...` in milliseconds. Calling `code_graph ranked` or `code_graph cycles` hangs long enough to be killed interactively. Suspected cause: the `warmed` flag reflects skeleton presence, but derived indices (PageRank, SCC, reverse-edge maps) are recomputed per call under a lock shared with the 45 s `spawn_blocking` warm pipeline. Under concurrent panel queries on Pulse mount, ops contend or redundantly re-derive.

These bugs are symptoms, not the decision. They are cited here because the ADR depends on them being fixed: redistributing architect's responsibilities is useless if architect sessions still cannot reach the graph.

### Pulse has no producer

The Pulse panels (Hotspots, Dead code, Cycles, Blast radius) mount against `code_graph` ops that expect architect-produced structural findings to be ready. Under ADR-034, architect spends most cycles on board-janitor work; under ADR-050, architect is *supposed* to produce these findings but — because of the two bugs above — does not actually do so. Pulse looks broken from the outside because its producer loop was never completed.

### Planner already owns decomposition — patrol is a natural extension

ADR-034 §6 gives the Planner the per-epic roadmap and wave-based decomposition. The Planner already thinks in terms of board shape, batches, and what work should exist. "Keep the board tidy" is a small extension of the role it already plays; it is not a new role. Human teams call this the foreman.

### Auto-dispatch triggers cause reentrance

ADR-034 §4 defines two deterministic coordinator rules:

- `epic_created` → create decomposition task → dispatch Planner.
- Last task in epic closed → create decomposition task → dispatch Planner.

Both rules fire unconditionally. An active Planner that is mid-intervention — force-closing stale tasks as part of a board reshape — trips the second rule and spawns a *second* Planner that competes with the one already running. The same failure mode exists when a Planner creates an epic mid-decomposition and the first rule spawns a fresh breakdown Planner against work the original is still shaping.

An audit of the current code (see "Audit findings" below) confirms the call sites:

- Epic creation → `server/crates/djinn-agent/src/actors/coordinator/actor.rs:554–566` → `maybe_create_planning_task()` at `coordinator/wave.rs:23` → `dispatch_ready_tasks()`.
- Task closed → `actor.rs:599` → `on_task_closed()` → `coordinator/rules.rs:42–157`.
- `close_reason` is a free-form TEXT column (`models/task.rs:99`, migration `V20260303000003__task_state_fields.sql:3`) with only two values in use today: `"completed"` and `"force_closed"`. Not rich enough to distinguish natural completion from reshape, but the storage is free-form so extension is cheap.
- Sessions carry `agent_type`, `task_id` (nullable), `project_id`, and `status` (`models/session.rs:28`). Scope for "is a Planner running on epic X?" is derivable via `session.task_id → task.epic_id`. No schema migration required.

## Decision

### 1. Role redistribution

| Responsibility | ADR-034 Owner | This ADR Owner |
|---|---|---|
| 5-min board patrol | Architect | **Planner** |
| Dedupe, reshape, force-close cleanup | Architect | **Planner** |
| Escalation ceiling above Lead | Architect | **Planner** |
| Spawning Planner for decomposition | Architect / coordinator | Coordinator only |
| Canonical graph reads (`code_graph` ops) | Architect | Architect (unchanged, per ADR-050) |
| ADR drafts, epic proposals, improvement tickets | Architect (in theory) | **Architect (as proposals, not direct board actions)** |
| Per-epic roadmap / wave decomposition | Planner | Planner (unchanged, per ADR-034 §6) |

The two roles now have orthogonal mandates:

- **Planner** — the foreman. Owns the board. Dedupes, reshapes, dispatches, unsticks. Runs the patrol. Breaks down epics into tasks. Handles all escalations from Lead. Can dispatch an Architect as a spike when design input is needed.
- **Architect** — the consultant. On-demand only. Reads the canonical graph (per ADR-050). Produces proposals: ADR drafts, epic shells, improvement tickets, spike findings. Does not close tasks, does not dispatch workers, does not run quality gates. Its only board-side side effect is creating items in dedicated `proposed` lanes.

The **Architect ↔ Chat parity contract** from ADR-050 §2 is preserved without change: chat is the human-facing interactive form of Architect. Every rule in this ADR that names "Architect" applies equally to Chat. The contract test asserting symmetric tool surfaces remains binding.

### 2. Architect triggers are on-demand only

Architect runs in exactly two circumstances:

1. **Planner spike.** Planner dispatches Architect when planner state alone cannot answer a design question. A spike is bounded, scoped to one question, and returns a report. Scope is one of: `epic`, `module`, or `project`. The spike prompt contains the question, the scope hint, and a reference to the dispatching Planner session.
2. **User ask.** User invokes "Ask architect" from Pulse (or from chat, which is already the interactive architect form per ADR-050). Same dispatch machinery, different originator.

The 5-minute cadence-driven architect patrol from ADR-034 §4 is **removed**. There is no architect cadence.

**Silent runs are prohibited.** A spike that produces no new findings must return an explicit "no new findings since last run at {timestamp}" report. Pulse renders this as a positive signal ("architect audited, nothing to flag"). Silent success is indistinguishable from silent failure and is not allowed.

### 3. Canonical graph warming is infrastructure, not agent work

This ADR formalizes what ADR-050 §3 implies but does not state outright: **the canonical graph is warmed by a server-managed pipeline, not by any agent.** Architect and Chat are *consumers* of the warm cache. They never warm it themselves.

- **Trigger policy** is unchanged from ADR-050 §3: lazy, single-flight, per `(project_id, origin/main commit_sha)`, in the dedicated `.djinn/index-tree/` worktree, acquired on first consumer demand.
- **Derived indices** (PageRank, SCC components used by `cycles`, reverse-edge maps used by `impact`, degree totals used by `ranked`) are computed once during warm and cached alongside the skeleton. Read ops read from the derived cache; they do not recompute. This directly fixes the `ranked`/`cycles` hang observed on 2026-04-08.
- **Agent bridge** exposes `code_graph` to the architect role. This directly fixes the "`code_graph not available in agent bridge`" log. Before this fix, the rest of this ADR has no value — architect sessions cannot produce the findings Pulse needs.
- The stale memory note `project_canonical_graph_warming_architect_only.md` (dated 2026-04-08, same day as this ADR) records the rule "only architect warms." That rule is replaced by "nobody agent-side warms — the server pipeline warms, workers tolerate stale skeleton." The spirit of the original fix (decoupling workers from the 45 s pipeline) is preserved; only the warmer changes.

### 4. Architect produces proposals, never direct board actions

Architect outputs land in Pulse as one of five artifact types. None of them are direct state transitions on existing work.

1. **Canonical graph freshness artifact** — `last_warm_at`, `pinned_commit`, `commits_since_pin`, counts of unreviewed proposals, last spike timestamp. Not strictly an agent output; produced by the warm pipeline and surfaced in the Pulse freshness strip.
2. **ADR proposal** — draft in a `proposed/` subdirectory under `.djinn/decisions/`. Carries: title, decision, alternatives, *why-now* (the structural trigger from the graph — e.g. "cycle detected between `foo` and `bar`"), `work_shape: none | task | epic`, and `originating_spike_id`. Drafts are not adopted ADRs; adoption is a user action.
3. **Epic proposal** — entry in a `proposed` lane for epics. Carries scope, rationale, and `originating_adr_id` if derived from an accepted ADR. Not a live epic.
4. **Improvement ticket** — low-priority finding in an `architect-suggested` lane. Source: `code_graph cycles`, `code_graph orphans`, ADR-drift edges from `code_graph edges` (per ADR-050 §4), god-object flags from `code_graph ranked` sorted by degree.
5. **Spike report** — bounded findings returned to the dispatching Planner (or user). Attached to the originating spike session. Can be empty ("no new findings") and that is a valid success — see §2.

Architect has **no mechanical side effects on the board**. It cannot close tasks, cannot transition status, cannot dispatch workers, cannot run quality gates, cannot create live epics or live tasks. Its creation surface is limited to the proposed lanes.

### 5. ADR-to-epic promotion pipeline

```
Architect spike (or user ask)
    ↓
ADR proposal  [+ optional epic/improvement proposals]
    ↓
User reviews in Pulse → accept / reject / defer
    ↓ (on accept, if work_shape ∈ {task, epic})
"Create epics from this ADR" button in Pulse
    ↓
Planner dispatched as conversion-planner
  • mission: "epic shells only, do not create tasks"
  • inputs: accepted ADR, originating spike report
    ↓
Conversion-planner creates N epics (auto_breakdown=true, default)
    ↓
Coordinator auto-dispatches breakdown Planner per epic (in parallel)
    ↓
Each breakdown Planner decomposes its epic (per ADR-034 §6 wave rules)
    ↓
Workers execute
```

Key properties:

- **Architect cannot manufacture work.** Every work item that exists on the live board is gated on user acceptance of a proposal, then planner action.
- **Conversion is itself a planner dispatch**, not a human mechanical step. The user clicks a button; a planner runs the ADR-to-epic translation. This is intentional — the planner is the only role allowed to create live epics, and acceptance of an ADR should not bypass that rule.
- **Breakdown is parallel.** Each epic created during conversion auto-dispatches its own breakdown Planner. They run concurrently on different epics. The conversion-planner's prompt is explicit: *"Create epic shells only. Do not create tasks in this run. The system will dispatch a separate Planner per epic for task breakdown."*
- **Originating ADR is threaded through.** Each epic carries `originating_adr_id`; each breakdown-planner session receives the ADR context in its prompt so downstream task creation is grounded in the decision rationale. Per our memory system, this is a high-value context plumb.

### 6. `auto_breakdown` flag on epic creation

Add `auto_breakdown: bool` to the epic creation API (MCP + internal), default **`true`**.

- **`true` (default)** — creating an epic fires the existing coordinator rule (ADR-034 §4) and auto-dispatches a breakdown Planner.
- **`false`** — caller claims ownership of breakdown. No auto-dispatch. Caller either creates tasks in the same session or leaves the epic as a shell for later.

The default is `true` because forgetting the flag is self-correcting (an extra Planner runs on an epic that already has tasks; it sees them and exits). The inverse default would create orphan epics with no breakdown, which is silently worse.

The opt-out exists for three cases:

1. **Manual user epic creation** with a "skip auto-planning" UI checkbox, when the user wants to author the breakdown themselves.
2. **Same-session breakdown** — rare, where a Planner has rich context from an ADR and decides to do the full job in one run (creates the epic and the tasks together).
3. **Bulk import / test** — replay scenarios where you do not want N Planner dispatches.

The conversion-planner from §5 uses the default (`true`) so breakdown is parallel per epic.

### 7. Auto-dispatch reentrance guards

Both auto-dispatch rules from ADR-034 §4 gain shared guards, implemented as a single helper called from both sites:

```rust
fn should_auto_dispatch_planner(scope: Scope, event: DispatchEvent) -> bool
```

Call sites:

- `coordinator/rules.rs:42` (inside `on_task_closed`) — for the "last task closed → dispatch planner" rule.
- `coordinator/wave.rs:23` (inside `maybe_create_planning_task`) — for the "epic created → dispatch planner" rule.

The helper applies three checks, in order:

1. **Close-reason filter** (task-close path only). Extend `close_reason` from its current values (`"completed"`, `"force_closed"`) with: `"reshape"`, `"superseded"`, `"duplicate"`. If the closing task's `close_reason` is one of the reshape values, **skip auto-dispatch**. Planner interventions that force-close tasks during a board reshape must set one of these reshape reasons at the close site — this is a prompt/convention update as well as a code change.
2. **`auto_breakdown = false`** (epic-create path only). Respected directly — if the caller opted out, the helper returns `false` regardless of other state.
3. **Active-session guard** (both paths). Query running sessions with `agent_type = "planner"` whose scope intersects the event scope. For epic-level events, scope is `epic_id`; derive from `session.task_id → task.epic_id`. For project-wide Planner interventions with `task_id = NULL`, treat as an umbrella over all epics in the project. If any match, **skip auto-dispatch**.

**Exit recheck.** When a Planner session ends, the helper re-evaluates the epics that session touched. If an epic is legitimately empty (natural completion, no other active Planner, epic still open), the auto-dispatch fires then instead of during the intervention. This closes the gap where a legitimate completion happened mid-intervention and was skipped.

**Stale safety net.** A background sweep (configurable interval, default 15 minutes) catches epics that fell through all three checks — empty, no active Planner, no pending recheck, epic still open. Defensive only; the primary mechanism is event-driven.

**Centralization is load-bearing.** Future auto-dispatch rules (planner on verification failure, planner on stalled session, etc.) must call the helper. This is the architectural move that prevents reentrance bugs from multiplying as the coordinator grows.

### 8. Escalation ladder

```
Worker fails                    → auto-reopen (up to 2 times, unchanged)
3rd failure                     → Lead intervention (unchanged, 10-min timeout)
Lead cannot resolve             → request_planner  → Planner intervention
2nd request_lead on same task   → skip Lead, route to Planner
Planner needs design input      → dispatch Architect spike (bounded, scoped)
Architect spike returns findings → Planner acts on findings
Architect alone cannot resolve  → comment on task; stays for human review
```

Changes from ADR-034 §8:

- The escalation ceiling is **Planner**, not Architect.
- `request_architect` is removed from Lead's toolbelt. `request_planner` replaces it. Lead never calls Architect directly.
- Architect is reachable only via (a) Planner dispatch (as a spike) or (b) user request from Pulse/chat. It is never an escalation *target* from the worker pipeline.

Rationale: Lead should not have to classify escalations as "scope issue" vs "design issue." Lead hands off to Planner, and Planner — which holds the board state — decides whether a spike is needed.

### 9. Pulse as the consumer surface

Pulse becomes the canonical UI for consuming Architect output and originating user asks. Panels map to producer sources as follows:

| Panel | Source | Producer |
|---|---|---|
| Freshness strip | `code_graph status` + proposal store + recent spike metadata | Warm pipeline + Architect |
| Hotspots | `code_graph ranked` (sort_by: degree or pagerank) | Warm pipeline (derived index) |
| Dead code | `code_graph orphans` | Warm pipeline (derived index) |
| Cycles | `code_graph cycles` | Warm pipeline (derived index) |
| Blast radius | `code_graph impact` on user-selected symbol | Warm pipeline (on demand) |
| ADR drift | `code_graph edges` with ADR-defined glob rules | Warm pipeline + ADR metadata |
| Proposed ADRs | `.djinn/decisions/proposed/*` | Architect |
| Proposed Epics | Epic table `status = proposed` | Architect |
| Improvement Tickets | `architect-suggested` lane | Architect |
| Spike history | Recent Architect sessions with kind=`spike` | Architect dispatch log |

Controls:

- **"Ask architect"** button — dispatches a user spike with the user-entered question.
- **"Create epics from this ADR"** button (on an accepted ADR) — dispatches the conversion-planner from §5.
- **Accept / reject / defer** on each proposal.

Empty-state behaviour: when the canonical graph is cold, Pulse shows a "warming" indicator with the pipeline's progress, not a blank loader. When a spike is in-flight, Pulse shows the spike's originating question and live status. When a recent spike returned "no new findings", Pulse renders that explicitly ("audited 5 minutes ago, nothing new since last run").

## Consequences

### Positive

- **Pulse finally has a producer loop.** Architect generates findings → Pulse renders → user accepts → Planner converts → Workers execute. The cycle closes. Today it is open at the producer end.
- **Architect cost is bounded and deliberate.** No 5-minute cadence burning tokens when the codebase is stable. Every run has a reason (a Planner spike with a question, or a user ask).
- **Planner-as-patrol matches existing shape.** Planner already decomposes and shapes the board. Adding "patrol" is an extension, not a new role.
- **Reentrance guards eliminate a class of auto-dispatch bug.** The `should_auto_dispatch_planner` helper centralizes the policy; future rules inherit correctness.
- **Lead escalation is simpler.** One target (Planner). No "is this scope or design?" classification at Lead level.
- **Architect value is measurable.** Silent runs are prohibited; every spike reports something (findings or explicit "nothing new"). The "is architect earning its keep?" question becomes answerable from Pulse alone.
- **Chat parity is preserved.** ADR-050's symmetric tool surface contract is unchanged. Chat remains the interactive form of Architect; changes here apply equally to both.
- **Proposals are a safe creation surface.** Architect can surface bold ideas without side effects. The user gate (accept/reject/defer) absorbs the risk of noise.

### Negative / Risks

- **No automatic "systemic failure caught within 5 minutes" from Architect.** ADR-034 §8 valued this explicitly. Mitigation: Planner inherits the patrol at the same (or lower) cadence; it is cheaper because it does not require code-reasoning context. Systemic failures that need *code* reasoning must be surfaced by Planner calling an Architect spike — an extra hop.
- **More steps in the ADR → work pipeline.** User now clicks through accept → convert → breakdown. Mitigation: each step has a clear Pulse checkpoint; user only participates at the proposal-acceptance boundary.
- **New `proposed` lane concepts.** Requires storage, UI, lifecycle (accept/reject/defer/expire), decay policy. This is additive but non-trivial.
- **Planner takes on more responsibility.** Risk: a single Planner role is overloaded (decomposition + patrol + intervention + conversion). Mitigation: the three cadences don't overlap in practice (decomposition is epic-triggered, patrol is periodic, intervention is escalation-triggered), and the active-session guard prevents Planners from stomping each other when they do.
- **`close_reason` discipline is a prerequisite.** Every close site that performs a reshape must set one of the new reshape reasons. Sites that forget will still trip auto-dispatch — the active-session guard is the second layer that catches them.
- **Proposal graveyard risk.** Unreviewed proposals accumulate. Mitigation: decay/expiration policy (exact rules TBD in a follow-up); for v1, proposals older than 14 days are hidden from the default Pulse view and swept after 30.
- **This ADR's value is gated on the two bugs being fixed first.** If `code_graph` never reaches architect through the bridge, or if read ops continue to hang, Architect produces nothing and the pipeline has no input. Migration step 1 and 2 are blocking.

### Neutral

- **Architect invocation count will change shape.** ADR-050 predicted a jump from 1 to many. Under this ADR, the volume depends on Planner spike frequency and user asks, not a cadence. Expect bursts (during active design) and quiet periods (during execution-heavy work).

## Alternatives considered

**Keep Architect as patrol, just fix the warming bugs.** Rejected. The two-jobs coupling is structural, not operational. Even with `code_graph` wired correctly, Architect continues doing board-janitor work that doesn't match its code-reasoning tools. The rename clarifies intent and makes role extensions easier (e.g., the ADR→epic pipeline would not fit naturally under "patrol Architect").

**Split Architect into `architect-patrol` and `architect-consultant` as two distinct roles.** Rejected. The patrol role belongs to Planner (it is already board-shaped). Adding a third role multiplies dispatch complexity for no gain.

**Cadence-driven Architect with a reason gate** ("run every N minutes, but only if the graph has changed"). Considered. ADR-050's commit-delta trigger already gives "run when origin/main advances." Adding a wall-clock cadence on top produces duplicate runs on quiet repos. Planner spike + user ask is sufficient; no cadence needed.

**Keep `lead → architect` escalation as a shortcut.** Rejected. Adds two paths to Architect (direct from Lead, indirect via Planner), and the "Planner knows when to call Architect" rule is cleaner than asking Lead to classify escalations.

**Staleness-only reentrance guard** (5-minute debounce on empty epics, no close-reason filter, no active-session check). Rejected. Requires a timer, introduces latency, and still fires mid-intervention if the intervention exceeds the debounce.

**Hard prohibition on epic creation during a Planner session** (no `auto_breakdown` flag). Rejected. Too blunt. Legitimate same-run epic creation (e.g. Planner splits a large epic mid-decomposition) would be blocked.

**Architect writes directly to the live board** (bypass proposals). Rejected. Architect's cost profile and error rate do not justify direct board writes. Proposals give the user a review gate at near-zero cost.

## Audit findings (2026-04-08)

Relevant file references discovered during this ADR's design, to anchor implementation:

- **`close_reason` schema**: `server/crates/djinn-core/src/models/task.rs:99`; column in `server/crates/djinn-db/migrations/V20260303000003__task_state_fields.sql:3`; nullable TEXT, free-form, not an enum.
- **Close sites**: `task.rs:503` (Close → `"completed"`), `task.rs:544` (ForceClose → `"force_closed"`), `task.rs:560` (UserOverride → `"force_closed"`), `task.rs:679` (PrMerge → `"completed"`), `task.rs:518` (Reopen → NULL).
- **Session model**: `server/crates/djinn-core/src/models/session.rs:28`. Fields include `agent_type`, `task_id: Option<String>`, `project_id`, `status`, `started_at`, `ended_at`. Scope for "active Planner on epic X" is derivable via `session.task_id → task.epic_id`.
- **Active session query**: `server/crates/djinn-db/src/repositories/session.rs:251` — existing method returns running sessions by project; needs extension (or companion method) that joins to tasks to filter by `epic_id`.
- **Auto-dispatch call sites**:
  - Epic created / drafting→open: `server/crates/djinn-agent/src/actors/coordinator/actor.rs:554–566` → `maybe_create_planning_task` at `coordinator/wave.rs:23`.
  - Task closed: `actor.rs:599` → `on_task_closed` → `coordinator/rules.rs:42–157`.
- **Patrol task evidence**: af5l ("Architect patrol: board health review") is a real task row with `issue_type: "review"`, `owner: "system"` — confirming the coordinator creates a synthetic task per patrol run (ADR-034 §7). Planner intervention sessions under this ADR follow the same pattern so they carry a `task_id` the active-session guard can query cleanly.

## Migration

**Prerequisites (blocking — Migration cannot proceed until these land):**

1. **Fix `code_graph` agent bridge exposure.** Wire `code_graph` through the agent bridge for the architect role. Investigate source of the "code_graph not available in agent bridge — use MCP server" log and add the missing tool registration. Without this, every downstream change in this ADR is inert.
2. **Fix read-op hangs on warmed cache.** Audit the `code_graph ranked` / `cycles` / `impact` code paths. Determine whether derived indices are cached or recomputed per call; determine whether the 45 s `spawn_blocking` pipeline holds a lock that contends with reads. Fix so that reads are pure in-memory lookups against derived-index caches built during warm.

**Role redistribution epic:**

3. Remove the 5-minute Architect patrol timer from coordinator rules (`actor.rs` tick loop). Add a Planner patrol timer in the same place, at the same or lower cadence.
4. Move board-health checks (throughput, force-close rate, stuck-task sweeps) from `prompts/architect.md` into `prompts/planner.md`. Update both prompts.
5. Replace `request_architect` in Lead's toolbelt with `request_planner`. Update `prompts/lead.md` accordingly.
6. Update `prompts/architect.md`: remove patrol/janitor directives; add "produce proposals, never direct board writes" contract; add silent-run prohibition. Update `prompts/chat.md` for parity.

**Reentrance guards epic:**

7. Extend `close_reason` values with `"reshape"`, `"superseded"`, `"duplicate"` at `models/task.rs`. Update close sites that perform reshape operations (primarily Planner-driven intervention force-closes).
8. Implement `should_auto_dispatch_planner(scope, event)` helper. Wire into `on_task_closed` (rules.rs:42) and `maybe_create_planning_task` (wave.rs:23). Add `active_planner_for_epic` repo method.
9. Implement exit-recheck: on planner session end, re-evaluate epics the session touched and fire auto-dispatch if appropriate.
10. Implement stale safety-net sweep (default 15 min interval).

**Proposal pipeline epic:**

11. Add `auto_breakdown: bool` to the epic creation API (internal + MCP surface). Default `true`. Wire through dispatch logic.
12. Add `proposed/` lanes for ADRs (filesystem under `.djinn/decisions/proposed/`), Epics (`status = proposed`), and improvement tickets (`architect-suggested` lane).
13. Implement proposal lifecycle (accept / reject / defer / expire). Wire into Pulse.
14. Wire Pulse panels to the producer sources listed in §9. Add "Ask architect" and "Create epics from this ADR" buttons.
15. Add the conversion-planner dispatch path: on accepted ADR with `work_shape ∈ {task, epic}`, dispatch a Planner with the "epic shells only" mission and the ADR context plumbed through.
16. Thread `originating_adr_id` through epic creation → breakdown Planner session context.

**Cleanup:**

17. Update the memory note `project_canonical_graph_warming_architect_only.md` to reflect the new rule ("server pipeline warms, no agent warms") once the warming pipeline is actually decoupled from the architect role.
18. Regenerate any affected `*_tools_section_snapshot.snap` files.

Prerequisites (1, 2) gate everything. Role redistribution (3–6) and reentrance guards (7–10) can proceed in parallel after prerequisites land. Proposal pipeline (11–16) depends on role redistribution. Cleanup (17, 18) is the last step.

## Implementation order

1. Land this ADR.
2. Prerequisite fixes (Migration 1 and 2) — blocking for everything downstream.
3. Role redistribution epic (Migration 3–6) and Reentrance guards epic (Migration 7–10) in parallel.
4. Proposal pipeline epic (Migration 11–16).
5. Cleanup (Migration 17–18).

## Relations

- [[ADR-034 Agent Role Hierarchy — Architect Patrol, Task Types, and Escalation]] — partially superseded. The patrol role, escalation ceiling, and architect dispatch triggers move. Wave-based decomposition, self-task creation, task types, and coordinator rules 1–2 (excluding the auto-dispatch guard changes) are preserved.
- [["ADR-050: Architect/Chat Code-Graph Consolidation, Canonical SCIP Indexing, and Graph Query Extensions"]] — extended. This ADR consumes ADR-050's tool surface and formalizes Architect as the sole producer of code-reasoning findings. The parity contract between Architect and Chat is preserved unchanged.
- [[ADR-046 Chat-Driven Planning — Drafting Epics, Research Agent Deliverables, and Memory Write Access]] — extended. Chat is the interactive form of Architect (per ADR-050); this ADR makes the symmetric consultant contract explicit.
- [[ADR-025 Backlog Grooming and Autonomous Dispatch Triggers]] — extended. Auto-dispatch rules gain reentrance guards.
- [[ADR-023 Cognitive Memory Architecture]] — complementary. Proposals are memory writes with an acceptance lifecycle; confidence scoring applies to adopted ADRs once they leave the `proposed/` lane.
- [[ADR-047 Repo-Graph Query Seam for code_graph tool]] — related. The bridge surface being fixed in Migration step 1 is the seam defined by this ADR.
- [[ADR-043 Repository Map — SCIP-Powered Structural Context for Agent Sessions]] — related. The canonical graph lifecycle is the infrastructure ADR-050 built on, which this ADR formalizes as "not an agent task."
