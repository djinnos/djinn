---
tags:
    - planning
    - roadmap
title: Roadmap
type: roadmap
---
# Roadmap — Djinn Server Rust Rewrite

Phased delivery plan for v1 requirements. Each phase builds on the previous and has testable success criteria. Phases are sequenced by real dependencies — later phases require earlier foundations.

## Progress Overview

_Updated: 2026-03-03 (post-audit)_

| Phase | Status | Remaining |
|-------|--------|-----------|
| Phase 1: Foundation | ✅ Complete | — |
| Phase 2: Task Board | ✅ Complete | — |
| Phase 3: Knowledge Base | ✅ Complete | — |
| Phase 4: Git Integration | ✅ Complete | — |
| Phase 5: Coordinator | ✅ Complete | — |
| Phase 6: Review | ✅ Complete | — |
| Phase 7: Desktop & Sync | 🟡 75% | `qhb4` |
| Phase 8: Session Visibility | ⚪ 0% | `yvw6`, `1x2s`, `32j6` |
| Phase 9: V1 Completion | ⚪ 0% | `cu4v`, `1upo`, `18a0`, `layi`, `lypu`, `1i5q`, `ewbt` |

**Original scope: 37/37 items closed (100%) — original phases complete**
**Post-audit: 44/55 items closed (80%) — 11 new items added from Go server gap analysis**

**ADR-008:** Goose library replaces summon. MCP-connect bridge (`1tst`) and scaffold system (`1nby`) dropped. See [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]].

**ADR-009:** Phases eliminated. No dispatch grouping — tasks dispatch when open + unblocked. Simplified execution tools (6 instead of 26). See [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]].

**ADR-010:** Session cost tracking. Per-task session history with token metrics for desktop visibility. See [[ADR-010: Session Cost Tracking — Per-Task Token Metrics]].


## Phase 1: Foundation — Database, Schema, and Core Server ✅

**Goal**: A running Axum server with the DB layer, migrations, and repository pattern. Nothing user-visible yet, but the foundation everything else is built on.

**Progress**: COMPLETE. Core foundation features closed: Axum server + rmcp (`cueu`), rusqlite+WAL (`642n`), refinery migrations (`3x7d`), repository pattern + broadcast events (`u0rv`).

**Requirements addressed**:
- DB-01 (rusqlite + WAL)
- DB-02 (connection discipline)
- DB-03 (WAL checkpoint)
- DB-04 (refinery migrations with timestamp naming)
- DB-05 (repository pattern with event emission)
- DB-06 (canonical schema.sql)
- DB-07 (UUIDv7)
- MCP-01 (Streamable HTTP transport)
- MCP-02 (per-session instances)
- MCP-05 (tool organization by domain)
- CFG-01 (settings in DB)
- CFG-02 (project registry)

**Success criteria**:
1. Server starts cleanly without auth token requirements
2. DB created at `~/.djinn/djinn.db` with WAL mode, refinery migrations applied
3. MCP transport accepts connections and serves a `system_ping` tool
4. Repository pattern enforced: `Connection` private, all writes emit events via broadcast channel
5. Settings and projects stored in DB with CRUD operations

**Depends on**: Nothing (first phase)

## Phase 2: Task Board

**Goal**: Full task board functionality — epics, tasks, state machine, blockers, activity log. The core domain model.

**Progress**: COMPLETE. All 8 features closed: Schema (`1la3`), state machine (`1fdt`), CRUD tools (`1e43`, `1ife`, `1e6d`, `1mw0`), transition/dispatch (`15p0`), blocker management (`1my3`), activity log + health (`xdr3`).

**Requirements addressed**:
- TASK-01 (epics as separate entities)
- TASK-02 (tasks under epics)
- TASK-03 (task state machine)
- TASK-04 (typestate at service layer)
- TASK-05 (blocker dependencies)
- TASK-06 (short IDs)
- TASK-07 (task CRUD via MCP tools)
- TASK-08 (activity log)
- TASK-09 (board health and reconciliation)
- TASK-10 (issue types: epic, feature, task, bug)
- TASK-11 (labels, priority, owner)
- TASK-12 (comments)
- TASK-13 (acceptance criteria)
- TASK-14 (design field)

**Success criteria**:
1. ~~Create epics and tasks via MCP tools; tasks enforce parent epic requirement~~ ✓
2. ~~Task state machine prevents illegal transitions at compile time (typestate)~~ ✓
3. ~~Blocker dependencies prevent task dispatch when blockers are open~~ ✓
4. ~~Short IDs generated and unique; resolvable in all task tools~~ ✓
5. ~~Activity log records all state changes with structured JSON payloads~~ ✓
6. ~~Board health detects stale tasks and stuck states~~ ✓

**Depends on**: Phase 1 (DB, MCP server, repository pattern)

## Phase 3: Memory / Knowledge Base

**Goal**: Full knowledge base — notes, FTS5 search, wikilink graph, memory↔task references.

**Progress**: COMPLETE. All 3 features closed: Note schema + FTS5 (`kt1l`), wikilink graph (`rw6q`), Note CRUD MCP tools (`1iyh`).

**Requirements addressed**:
- MEM-01 (typed notes with folders)
- MEM-02 (FTS5 search)
- MEM-03 (wikilink graph)
- MEM-04 (memory↔task references)
- MEM-05 (auto-generated catalog)
- MEM-06 (note CRUD via MCP tools)
- MEM-07 (git history tracking)
- MEM-08 (singleton types)
- MEM-09 (orphan detection)
- MEM-10 (broken link detection)

**Success criteria**:
1. ~~Create, read, edit, search, delete notes via MCP tools~~ ✓
2. ~~FTS5 search returns ranked results with snippets~~ ✓
3. ~~Wikilinks resolved bidirectionally; graph endpoint returns all edges~~ ✓
4. ~~Memory↔task references work in both directions (task → notes, note → tasks)~~ ✓
5. ~~Catalog auto-generated from index; orphans and broken links detected~~ ✓

**Depends on**: Phase 2 (task board — for memory↔task references)

## Phase 4: Git Integration ✅

**Goal**: Full git automation — worktrees, branch management, squash-merge, hook awareness.

**Progress**: COMPLETE. Epic `a7cx` closed. All 3 features delivered: GitActor (`5tl5`), worktree lifecycle (`f1sq`), branch management & squash-merge (`15wc`). Completed in parallel with Phase 1, ahead of original sequencing.

**Requirements addressed**:
- GIT-01 (task branches from target branch)
- GIT-02 (worktree isolation)
- GIT-03 (squash-merge on approval)
- GIT-04 (GitActor serialization)
- GIT-05 (hybrid git2 + CLI)
- GIT-06 (worktree lifecycle)
- GIT-07 (hook awareness)
- GIT-08 (target branch per project)
- CFG-03 (git settings per project)

**Success criteria**:
1. ~~GitActor serializes all operations; no concurrent git commands~~ ✓
2. ~~Worktrees created/removed cleanly; orphan detection works~~ ✓
3. ~~Squash-merge produces clean commit on target branch~~ ✓
4. ~~Pre-commit hook failures captured and reported (not silently swallowed)~~ ✓
5. ~~Target branch configurable per project; defaults to main~~ ✓

**Depends on**: Phase 1 (DB, settings). Completed early — ran in parallel with Phase 1.

---

## Phase 5: Agent Orchestration (Coordinator)

**Goal**: The coordinator dispatches agents to tasks, manages model health, handles graceful shutdown. The "brain" of the system. **ADR-008: Agents run in-process via Goose library, not as subprocesses via summon.**

**Progress**: COMPLETE. All 3 features closed: CoordinatorActor (`1u1b`), model health (`n8e4`), AgentSupervisor with Goose harness (`d9s4`). Tasks: credential vault (`sy31`), Goose integration (`1tal`), prompt templates (`lnfo`), extension tools (`ewkl`), supervisor actor (`rvmf`), dispatch flow (`xq5l`), graceful shutdown (`951w`).

**Requirements addressed**:
- AGENT-01 (actor hierarchy — sessions are tokio tasks, not subprocesses per ADR-008)
- AGENT-02 (three agent types)
- AGENT-03 (revised: dispatch via Goose library, in-process async tasks)
- AGENT-04 (model discovery — delegated to Goose's provider system)
- AGENT-05 (model health / circuit breakers — Djinn wraps Goose providers)
- AGENT-06 (session limiting)
- AGENT-07 (event-driven dispatch)
- AGENT-08 (stuck detection)
- AGENT-09 (revised: CancellationToken to Goose agents, not SIGTERM/SIGKILL)
- AGENT-10 (WIP commits on pause)
- AGENT-11 (actor struct limits)
- ~~AGENT-12 (scaffold system)~~ — DROPPED per ADR-008
- CFG-04 (narrowed: capacity/routing only, credentials in vault)
- AGENT-16 (credential vault in Djinn DB)
- AGENT-17 (Goose provider creation from vault at dispatch time)
- AGENT-18 (per-session Goose Agent configuration)
- AGENT-19 (OAuth-capable providers exposed/configurable via MCP)

**Success criteria**:
1. ~~Coordinator dispatches Goose agent to an open task; agent works in worktree~~ ✓
2. ~~Model health tracks failures; circuit breaker trips after threshold; reroutes to alternative~~ ✓
3. ~~Session limiting enforces per-model capacity~~ ✓
4. ~~Graceful shutdown: CancellationToken → Goose agent stops → WIP commit → worktree preserved~~ ✓
5. ~~Stuck detection recovers tasks from unresponsive agents within 30s~~ ✓
6. ~~Credential vault stores API keys; Goose providers created from vault at dispatch time~~ ✓
7. ~~Per-session prompt and extension configuration for different agent types~~ ✓
8. OAuth-capable Goose providers are discoverable/configurable over MCP and can dispatch without manual API key entry

**Depends on**: Phase 2 (task board for dispatch), Phase 4 (git for worktrees)


## Phase 6: Review System

**Goal**: Task review and phase review agents verify quality before approval. **Runs as Goose sessions per ADR-008.**

**Progress**: COMPLETE. Review agents (`lm7a`) closed. Scaffold system (`1nby`) dropped per ADR-008. Coordinator dispatches review agents for tasks in `needs_task_review` and `needs_phase_review` states. Supervisor handles transitions for all three agent types (worker, task_reviewer, phase_reviewer).

**Requirements addressed**:
- REVIEW-01 (task review: AC verification + code nitpicks)
- REVIEW-02 (epic review: completeness + aggregate quality)
- REVIEW-03 (rejection → rework loop)

**Success criteria**:
1. ~~Task review Goose agent checks acceptance criteria against code diff; approves or rejects with feedback~~ ✓
2. ~~Epic review Goose agent reviews aggregate diff for patterns/duplication~~ ✓
3. ~~Rejected tasks return to agent with feedback; agent reworks and resubmits~~ ✓
4. ~~Full review cycle: work → task review → approve/reject → close~~ ✓

**Depends on**: Phase 5 (coordinator for agent dispatch), Phase 4 (git for diffs)


## Phase 7: Desktop Integration and Sync

**Goal**: SSE change feed, direct DB reads, task sync via git branch, server lifecycle management. Desktop can consume the full system.

**Progress**: 3/4 features closed. SSE change feed (`ywb0`), djinn/ namespace sync (`2up9`) completed. MCP-connect bridge (`1tst`) dropped per ADR-008. Remaining: Server lifecycle (`qhb4`).

**Requirements addressed**:
- MCP-04 (SSE change feed with full entities)
- ~~MCP-03 (MCP-connect bridge mode)~~ — DROPPED per ADR-008
- DB-05a (desktop initial load via direct DB read)
- SYNC-01 (task sync via djinn/tasks branch)
- SYNC-02 (fetch-rebase-push)
- SYNC-03 (backoff on failures)
- SYNC-04 (enable/disable per-machine)
- WSL-01 (bind 0.0.0.0)
- WSL-02 (Linux filesystem)
- WSL-03 (HTTP over TCP)
- WSL-04 (runtime detection of direct DB access)
- LIFE-01 (revised: desktop-spawned OR standalone server modes)
- LIFE-02 (revised: graceful shutdown with Goose CancellationToken)
- LIFE-03 (graceful restart for updates)
- LIFE-04 (board reconciliation on startup)
- LIFE-05 (desktop monitors server process)

**Success criteria**:
1. ~~Desktop connects via SSE; receives full-entity events for all mutations~~ ✓
2. ~~Desktop reads DB file directly for initial load (local mode)~~ ✓
3. ~~Task sync exports/imports via djinn/tasks git branch with conflict resolution~~ ✓
4. Server lifecycle: desktop-spawned daemon, standalone VPS mode, graceful restart, board reconciliation
5. server.json discovery file written on startup

**Depends on**: Phase 2 (tasks), Phase 3 (memory), Phase 5 (coordinator events)


## Phase 8: Session Visibility & Cost Tracking

**Goal**: First-class session tracking with real-time desktop visibility and token/cost metrics. Sessions are viewable entities — users see active sessions in real-time, browse session history per task, and track cost across runs. **See ADR-010.**

**Progress**: 0/3 features. Epic `1ioz` created.

**Features**:
- `yvw6` — Session schema, repository, and lifecycle events (P0)
- `1x2s` — Token metrics capture from Goose sessions (P1, blocked by `yvw6`)
- `32j6` — Session MCP tools and task_show enrichment (P1, blocked by `yvw6`)

**Requirements addressed**:
- AGENT-19 (NEW: session persistence with token metrics)
- OBS-01 (extends: session events in activity stream)
- AGENT-15 foundation (v2: compute governance / ACU budgets)

**Success criteria**:
1. Sessions table tracks every agent session with task_id, model_id, agent_type, tokens_in/out
2. Supervisor writes session records on dispatch start and completion
3. SSE events emitted for session lifecycle (started, completed, interrupted, failed)
4. task_show includes active session and session count
5. session_list/session_active MCP tools return session data for desktop

**Depends on**: Phase 5 (supervisor writes sessions)

## Phase 9: V1 Completion — Audit Gaps

**Goal**: Close gaps found in the Go server comparison audit. Covers execution control, project tools, operational logging, conflict resolution, structured output parsing, merge tracking, and file watchers. **See ADR-009 for simplified execution model.**

**Progress**: 0/8 items. Epic `1hcn` created.

**Features/Tasks**:
- `cu4v` — Simplified execution control MCP tools (P0) — 6 tools per ADR-009
- `1upo` — Project management MCP tools (P0) — projects_add/list/remove
- `18a0` — Operational logging with file rotation (P1) — tracing-appender + system_logs tool
- `layi` — Conflict resolution merge flow (P1, blocked by `lypu`) — squash-merge on approval, conflict → reopen → resolve → retry
- `lypu` — Structured agent output parsing (P1) — worker DONE/BLOCKED, reviewer VERIFIED/REOPEN verdict extraction
- `1i5q` — Store merge_commit_sha on task after squash-merge (P2) — GIT-09
- `ewbt` — File watchers for KB and settings changes (P2) — notify crate, re-index on external edits
- `stdio-bridge` — `djinn-server --mcp-connect` stdio↔HTTP MCP bridge mode (P2) — plugin compatibility via daemon-discovered upstream URL

**Requirements addressed**:
- GIT-09 (merge_commit_sha on task)
- OBS-02 (file-based operational log)
- CFG-02 (project registry MCP tools — tools were missing, repo existed)
- REVIEW-01/02/03 (structured output parsing completes review flow)

**Success criteria**:
1. Desktop can start/pause/resume/kill execution via 6 MCP tools
2. Desktop can manage projects via 3 MCP tools
3. Logs written to ~/.djinn/logs/ with rotation; accessible via system_logs tool
4. Merge conflicts detected and resolved via agent rework loop
5. Agent verdicts parsed from output stream and drive state transitions
6. merge_commit_sha stored on task after successful squash-merge
7. External KB edits detected and re-indexed automatically

**Depends on**: Phase 5 (coordinator/supervisor for execution tools), Phase 4 (git for conflict resolution)

## Phase Dependency Graph

```
Phase 1-4: Foundation, Task Board, KB, Git ✅ (all complete)
    ↓
Phase 5: Coordinator ✅
    ↓
Phase 6: Review ✅
    ↓
Phase 7: Desktop & Sync 🟡 (qhb4)
    ↓
Phase 8: Session Visibility ⚪ (3 features)
    ↓
Phase 9: V1 Completion ⚪ (7 items)
```

Phase 8 depends on Phase 5 (supervisor writes sessions). Phase 9 is independent — can run in parallel with Phase 7/8 where items don't overlap. Within Phase 9: `layi` (conflict resolution) is blocked by `lypu` (structured output parsing).


## Coverage Check

Updated post-audit. ADR-009 eliminates phases (26 tools → 6). ADR-010 adds session tracking. New requirement AGENT-19 (session persistence).

- Phase 1: DB-01..07, MCP-01/02/05, CFG-01/02 (13 reqs) ✅
- Phase 2: TASK-01..14 (14 reqs) ✅
- Phase 3: MEM-01..10 (10 reqs) ✅
- Phase 4: GIT-01..08, CFG-03 (9 reqs) ✅
- Phase 5: AGENT-01..11, AGENT-16..18, CFG-04 (15 reqs) ✅
- Phase 6: REVIEW-01..03 (3 reqs) ✅
- Phase 7: MCP-04, DB-05a, SYNC-01..04, WSL-01..04, LIFE-01..05 (16 reqs)
- Phase 8: AGENT-19, OBS-01 extension (2 reqs)
- Phase 9: GIT-09, OBS-02, CFG-02 MCP tools, REVIEW-01..03 completion (5 reqs)
- Cross-cutting: TEST-01..03 (3 reqs)

Total: 94 (88 prior + AGENT-19 + 5 gap-identified coverage gaps) ✓


## Relations

- [[Project Brief]] — vision and scope defining the roadmap
- [[V1 Requirements]] — requirements consumed by each phase
- [[Research Summary]] — synthesis informing phase sequencing
- [[Database Layer — rusqlite over libsql/Turso]] — ADR-002 driving DB and desktop integration phases
- [[Migrations — refinery with timestamp-based naming]] — ADR-003 driving migration approach in Phase 1
- [[Server Lifecycle — Desktop-Managed Daemon with Graceful Restart]] — ADR-005 driving lifecycle in Phase 7
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — ADR-008 driving Phases 5-7
- [[Agent Harness Scope]] — scope boundaries for Goose integration
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — ADR-009 driving Phase 9 execution tools
- [[ADR-010: Session Cost Tracking — Per-Task Token Metrics]] — ADR-010 driving Phase 8 session visibility


## Traceability

| Requirement Category | Primary Research Source |
|---|---|
| MCP-* | Stack Research (rmcp patterns), Architecture Research §6 |
| DB-* | ADR-002, Stack Research (libsql patterns adapted), Pitfalls Research §4 |
| TASK-* | Architecture Research §3 (state machine), §5 (DB schema) |
| MEM-* | Brief (scope section) |
| AGENT-* | Architecture Research §1 (actors), Features Research (topology), Pitfalls Research §2 |
| REVIEW-* | Features Research (Planner/Worker/Judge), Brief |
| GIT-* | Architecture Research §4 (git2 + CLI), Pitfalls Research §6 |
| SYNC-* | Brief (scope section) |
| OBS-* | Brief, Features Research (89% need tracing) |
| LIFE-* | ADR-005 (Server Lifecycle) |
| TEST-* | Research Summary, Stack Research |
| CFG-* | Brief (scope section) |
| WSL-* | Architecture Research §7 (WSL considerations), ADR-002 |
