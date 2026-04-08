---
title: "ADR-051: Planner-as-Patrol, Architect-as-Consultant, ADR-to-Epic Pipeline, and Auto-Dispatch Reentrance Guards"
type: adr
tags: ["adr","architecture","agents","roles","planner","architect","patrol","auto-dispatch","reentrance","adr-pipeline","pulse"]
---



# ADR-051: Planner-as-Patrol, Architect-as-Consultant, ADR-to-Epic Pipeline, and Auto-Dispatch Reentrance Guards

## Status: In Progress (Draft)

Date: 2026-04-08

Supersedes (partially): [[ADR-034 Agent Role Hierarchy — Architect Patrol, Task Types, and Escalation]]
Extends: [["ADR-050: Architect/Chat Code-Graph Consolidation, Canonical SCIP Indexing, and Graph Query Extensions"]], [[ADR-046 Chat-Driven Planning]]
Related: [[ADR-025 Backlog Grooming and Autonomous Dispatch Triggers]], [[ADR-023 Cognitive Memory Architecture]], [[ADR-043 Repository Map — SCIP-Powered Structural Context]], [[ADR-047 Repo-Graph Query Seam]]

## Implementation Status (2026-04-08 — handoff to next session)

This section is a living checklist of what has landed, what is partially landed, and what remains. The decision and design below this section are unchanged from the original draft; only this status block evolves as work ships. **A future session resuming this ADR should read this section first**, then jump to the Migration section to pick up the next item.

### Landed on `main`

| Migration step | Commit(s) | What it does |
|---|---|---|
| **Step 1** — agent bridge `code_graph` exposure | [`7886bcc4`](#) | Replaces the `AgentRepoGraphOps` stub at `server/crates/djinn-agent/src/context.rs:156–239` with an injected `Arc<dyn RepoGraphOps>` plumbed from `AppState::agent_context()` in `server/src/server/state/mod.rs`. Architect sessions now actually reach the real `RepoGraphBridge`. Contract: `djinn-agent` keeps zero dependency on the `server` crate (per ADR-047) — the concrete bridge is injected at the server boundary. |
| **Step 2 (a)** — derived index caching | [`7a6f54c3`](#) | Extends `CachedGraph` in `server/src/mcp_bridge.rs` with `pagerank: Arc<RepoGraphRanking>` and `sccs: Arc<CachedSccs>` (one set per `kind_filter` variant: `None` / `File` / `Symbol`). Both populated inside the existing `spawn_blocking` boundary in `ensure_canonical_graph` so warm pays the CPU once. New `derive_graph_caches` helper + `load_cached_artifact` async helper that wraps both bincode-deserialise and derivation in `spawn_blocking` for the persistent `repo_graph_cache` hit paths. New `build_graph_with_caches_for_project` sibling of `build_graph_for_project` exposes the cached `Arc`s to read ops. `ranked` reads `cached.pagerank`; `cycles` reads the per-kind cached set and applies `min_size` at lookup time. Latency on warm reads drops from tens-of-seconds to sub-millisecond. |
| **Step 2 (b)** — Pulse no-rebuild | [`a5f38130`](#) (user-authored) | Splits `ensure_canonical_graph` into a refresh path (background warmer + chat first-use only) and a pure-read `read_cached_canonical_graph` path used by `build_graph_for_project` and `build_graph_with_caches_for_project`. Pulse panel reads no longer trigger `fetch_if_stale + reset_to_origin_main` and never fall through to a 30-minute SCIP rebuild when local main has advanced past the architect's last warm. Cold-cache reads return `GRAPH_NOT_WARMED_ERR` so Pulse renders its empty state instead of wedging. New regression tests under `ensure_canonical_graph_tests`. |
| **Step 5** — `request_architect` → `request_planner` | [`dfdf5b3f`](#) | Lead's toolbelt now exposes `request_planner` (was `request_architect`). The 2nd-escalation auto-route in `call_request_lead` and the PR-poller's "exceeded N rounds without approval" path both call `dispatch_planner_escalation` (was `dispatch_architect_escalation`), which creates a `review` task with `agent_type=planner` instead of `architect`. Per ADR-051 §8 the Planner is now the escalation ceiling above Lead. |
| **Step 4 partial / Step 6 partial** — architect & chat contracts | [`dfdf5b3f`](#) | Top-of-file "Role transition (ADR-051)" notice added to `prompts/architect.md` and `prompts/chat.md` (parity per ADR-050 §2). Both gain Contract 1 (proposals only — ADR drafts target `decisions/proposed/`, suggested epics + improvement tickets are embedded in the ADR draft, no `epic_create` for new architect-discovered work) and Contract 2 (silent runs prohibited — every spike must return findings or an explicit "no new findings"). `prompts/lead.md` gains a "Beyond Lead scope — request the Planner" paragraph. The patrol body of `architect.md` is unchanged below the new notice; the full content migration is the deferred half of Epic A. |
| **Epic B — Steps 7–10** — auto-dispatch reentrance guards | _uncommitted_ (2026-04-08, session handoff) | (7) `close_reason` literals extended via new `CLOSE_REASON_*` constants in `server/crates/djinn-core/src/models/task.rs` — adds `RESHAPE`, `SUPERSEDED`, `DUPLICATE` alongside `COMPLETED` / `FORCE_CLOSED`. Existing transition sites (lines 503/544/560/679) now reference the constants. No close site *emits* the new reasons yet — that's a Planner prompt/convention change for later; the active-session guard is the real protection today. (8) New `should_auto_dispatch_planner(db, event)` helper at `server/crates/djinn-agent/src/actors/coordinator/reentrance.rs` with `DispatchEvent::{TaskClosed, EpicCreated}`. Logic ordered: close-reason filter → `auto_breakdown` filter → active-session guard via new `SessionRepository::active_planner_for_epic(epic_id)` on `server/crates/djinn-db/src/repositories/session.rs` (joins `sessions → tasks` for `running` + `agent_type='planner'`). Wired into `on_task_closed` rules 1 & 2 at `coordinator/rules.rs` and into `maybe_create_planning_task` at `coordinator/wave.rs` (hardcoded `auto_breakdown: true` until Epic C plumbs the real flag). (9) Exit-recheck: new `("session", "completed"|"interrupted"|"failed")` event arm in `coordinator/actor.rs` → `handle_planner_session_ended` → `recheck_epic_after_planner_end` fires a `"post_planner_recheck"` dispatch if the epic is now eligible. New `epic_is_eligible_for_next_wave` helper in `rules.rs` shared by recheck + sweep. (10) 15-minute `sweep_stale_auto_dispatches` method wired into the existing tick loop at `actor.rs` next to `last_stale_sweep`, with new `AUTO_DISPATCH_SWEEP_INTERVAL` constant in `coordinator/types.rs` and `last_auto_dispatch_sweep: StdInstant` field on `CoordinatorActor`. **Tests**: 4 unit tests on the helper (one per decision branch), 2 repo tests on `active_planner_for_epic`, 1 integration test `batch_completion_suppressed_when_active_planner_on_epic` in `rules.rs`. `cargo check -p djinn-agent -p djinn-server` clean; `cargo test -p djinn-db session` 17 pass; `cargo test -p djinn-agent coordinator::` 60 pass including the new tests. |

**Commits to look up with `git show <sha>` for context**: `7886bcc4`, `7a6f54c3`, `a5f38130`, `1038fb62` (this ADR initial draft), `dfdf5b3f` (Epic A v1).

### Deferred — Epic A v2 (the prompt-content half)

The Epic A commit `dfdf5b3f` shipped the routing change and the contract additions, but **the full prompt content migration was deferred** because the patrol body is large and was tangled with the architect's existing corrective-action workflows. Specifically:

- **Migration step 3 — coordinator patrol timer rename.** The current code still has `maybe_dispatch_architect_patrol`, `DEFAULT_ARCHITECT_PATROL_INTERVAL`, `MIN_ARCHITECT_PATROL_MINUTES`, `MAX_ARCHITECT_PATROL_MINUTES`, and `CoordinatorMessage::TriggerArchitectPatrol` (test-only) in `server/crates/djinn-agent/src/actors/coordinator/`. These need renames to `*_planner_*` and the dispatched `agent_type` flips from `"architect"` to `"planner"`. The patrol task issue type (`"review"`) probably stays unchanged. Rough scope: ~10 identifier renames across `actor.rs`, `dispatch.rs`, `handle.rs`, `messages.rs`, `rules.rs`, plus regenerating any affected snapshots. The patrol-task title check at `actor.rs:592` (`task.title.contains("patrol")`) is role-agnostic and stays.
- **Migration step 4 — board-health content migration architect.md → planner.md.** The existing `architect.md` patrol body covers: board overview (§1), epic health check (§2), approach viability (§3), stuck work detection (§4), memory health (§5), contradiction review (§6), agent effectiveness review (§10), corrective actions (stuck task / wrong sequencing / missing blockers / empty epic). All of these are *patrol* responsibilities under ADR-051 and should move to `planner.md`. The `code_graph`-driven sweep (§7) and strategic ADR gaps (§8) and spike findings memory writes (§9) stay with the architect — those are the consultant duties.
- **Migration step 6 — strip patrol body from architect.md once §3+§4 complete.** Architect's prompt should end up containing only: identity, the two contracts (already added), the `code_graph` sweep, ADR/epic proposal authoring, spike findings memory writes, and the escalation ceiling rules. The corrective-actions section (§10 of the current prompt) goes away — architect no longer takes corrective actions on the live board.
- **`prompts/planner.md` requires a "patrol mode" branch.** The current planner prompt is single-mode (per-epic decomposition only). It needs to detect from the task context whether it was dispatched for decomposition (existing) or for a patrol/intervention (new) and run the appropriate workflow. The architect's patrol task-detection pattern (`task.issue_type == "review" && task.title.contains("patrol")`) is the model.

Why deferred: the routing-and-contracts slice (the part that landed) is high-leverage and self-contained — it changes Lead's escalation routing immediately and tells future architect runs to behave as consultants even though the patrol body is still verbatim. The content migration is bigger and would benefit from a focused session that rewrites both prompts together with a clear before/after comparison.

### Not started

**Epic C — Proposal pipeline backend (Migration steps 11–16).**

Subagent dispatched once, died from 529. Worktree set up at `djinn-adr51-proposals` and removed during cleanup. The hardest thing to keep correct here is the live-coordinator safety: **proposed-status epics MUST be excluded from the existing `epic_created → maybe_create_planning_task → dispatch breakdown planner` rule**, otherwise the entire proposal lane evaporates the moment an architect proposes an epic. Order of operations for the next session:

1. Add `Proposed` to `EpicStatus` enum + state machine (find via `grep EpicStatus`; probably `server/crates/djinn-core/src/models/epic.rs`).
2. **First**, audit and tighten the coordinator rules at `coordinator/actor.rs:554–566` and `coordinator/wave.rs:23` so they exclude `Proposed`. This is the safety guarantee — verify it works (with a unit test) before touching anything else.
3. Add `auto_breakdown: bool` flag (default `true`) on epic creation API. Plumb through `epic_create` MCP tool. Respect it in `maybe_create_planning_task` (early-return when `false`).
4. Add `originating_adr_id: Option<String>` column on epics. New refinery migration in `server/crates/djinn-db/migrations/V*.sql` (look at `V20260303000003__task_state_fields.sql` for the convention). Plumb through epic model + creation API.
5. Add `.djinn/decisions/proposed/` storage helpers (filesystem CRUD + frontmatter parsing for `work_shape`, `originating_spike_id`). Look at `server/crates/djinn-mcp/src/tools/memory_tools/` (`writes.rs`, `delete_ops.rs`, `move_ops.rs`) for the existing file-manipulation patterns.
6. Conversion-planner dispatch path: a new MCP tool (e.g. `propose_adr_accept`) or coordinator rule that fires on acceptance and dispatches a Planner with mission "convert this ADR into epic shells, do NOT create tasks". Threads accepted ADR content through to the breakdown-planner session context.
7. Wire `originating_adr_id` into breakdown-planner session context so downstream task creation has the ADR rationale.

**Live-coordinator safety rule (re-emphasized)**: under no circumstances may any code path in this epic call `mcp__plugin_djinn_djinn__epic_create` against a live MCP server. Tests must use in-process fixtures (`create_test_db`, `AppState::new(test_db, ...)`, the patterns in `server/src/mcp_bridge.rs` test modules and `server/crates/djinn-agent/src/test_helpers.rs`). The local Djinn coordinator runs in the background and will react to live epic creation by spawning real planner sessions — exactly the reentrance bug Epic B is meant to fix.

### Operational notes from this session

- **API was unstable on 2026-04-08.** Six of seven background subagent dispatches died on `529 Overloaded` (Epic B retry, Epic C, Epic A retry, two earlier Fix 2 attempts, and the original Epic A subagent that did manage 27 tool uses but was eventually killed). The one subagent that succeeded end-to-end was Fix 1 (agent bridge). Fix 2 (derived caches) was completed manually by the parent agent. Epic A v1 was completed manually by the parent agent in the dedicated worktree. **If the API is still unstable, the next session should expect to do this work directly rather than chasing subagent retries.**
- **Worktree isolation behaviour to know**: when `Agent` tool dispatches were called with `isolation: "worktree"`, the file edits ended up in the parent agent's working directory rather than a separate worktree. The next session should not rely on `isolation: "worktree"` for hard separation; manually create worktrees with `git worktree add` and instruct subagents to `cd` into the explicit path.
- **`cargo insta accept --workspace`** is the right way to update snapshots after this kind of prompt/tool rename. `INSTA_UPDATE=auto cargo test ...` does *not* auto-accept; it leaves `.snap.new` files and the test still fails.
- **`Quality Gate` bypass on push** is authorized for ADR-051 work per the standing memory note (`feedback_merge_directly_bypass_quality_gate.md`). Local `cargo check -p djinn-agent -p djinn-server` is the verification expected before each push.
- **Memory note `project_canonical_graph_warming_architect_only.md` is now operationally stale.** With Step 2 (b) `read_cached_canonical_graph` landed, non-warming readers no longer touch the warming pipeline at all. The note should be updated to reflect "background warmer is the only path that triggers SCIP work; reads are pure cache lookups via `read_cached_canonical_graph`; cold-cache reads return `GRAPH_NOT_WARMED_ERR`". Defer the memory update until after Epic B and C are designed (the rule may evolve again).

### Recommended next-session order

Epic B (auto-dispatch reentrance guards) landed in this session as uncommitted edits; commit + verify before proceeding. Remaining work:

1. **Epic A v2** (coordinator patrol timer rename + full content migration architect.md → planner.md). Best done in a focused session because the prompt rewrite is judgment-heavy.
2. **Epic C** (proposal pipeline backend). Largest scope; Epic B's active-session guard is the safety net Epic C depends on (already in place). Live-coordinator safety rule from the Epic C notes still applies — no live `epic_create` calls during implementation.
3. **Pulse panels** (frontend) come after Epic C. They consume the proposal lanes Epic C creates.
4. **Memory note update** (`project_canonical_graph_warming_architect_only.md`) and **stale snapshot cleanup** (the duplicate `request_architect` snapshots under `server/crates/djinn-agent/src/snapshots/` that Epic A v1 left untouched) are housekeeping that can ride along with any of the above.

**Known loose ends from Epic B that the next session should track**:
- Close sites still emit only `"completed"` / `"force_closed"`; the new `CLOSE_REASON_{RESHAPE,SUPERSEDED,DUPLICATE}` constants exist but are not emitted. The Planner prompt update (part of Epic A v2's content migration) should introduce the convention, and any code path that force-closes during a reshape should set `CLOSE_REASON_RESHAPE` etc.
- `auto_breakdown: true` is hardcoded at the `wave.rs` call site. Epic C must plumb the real value from the epic creation API into `DispatchEvent::EpicCreated`.
- No end-to-end test for `recheck_epic_after_planner_end` or `sweep_stale_auto_dispatches`; they compile and are wired, but coverage is via the helper's unit tests only. If desired, drive `SessionRepository::update(SessionStatus::Completed)` while a coordinator is spawned — the raw `CoordinatorActor` test seam at the bottom of `coordinator/mod.rs` is the cleanest hook.
- Epic B's rule-2 path in `on_task_closed` was NOT dedup'd against the new `epic_is_eligible_for_next_wave` helper — a judgment call to minimize churn. If preferred, replace the inlined block.

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
