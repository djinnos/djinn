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

_Updated: 2026-03-03_

| Phase | Status | Remaining |
|-------|--------|-----------|
| Phase 1: Foundation | 🟡 85% | `168f` Clerk JWT, `u0rv` Repo pattern |
| Phase 2: Task Board | ⚪ 0% | 6 features: `1la3` `1fdt` `1e43` `15p0` `1my3` `xdr3` |
| Phase 3: Knowledge Base | ⚪ 0% | 3 features: `kt1l` `rw6q` `1iyh` |
| Phase 4: Git Integration | ✅ Complete | — |
| Phase 5: Coordinator | ⚪ 0% | 3 features: `1u1b` `d9s4` `n8e4` |
| Phase 6: Review | ⚪ 0% | 2 features: `1nby` `lm7a` |
| Phase 7: Desktop & Sync | ⚪ 0% | 4 features: `ywb0` `qhb4` `1tst` `2up9` |

**Overall: 17/37 tasks closed (46%) — Critical path: `u0rv` → `1la3` → `1fdt` → `15p0` → `{1u1b, d9s4}`**

## Phase 1: Foundation — Database, Schema, and Core Server 🟡

**Goal**: A running Axum server with the DB layer, migrations, repository pattern, and Clerk JWT authentication. Nothing user-visible yet, but the foundation everything else is built on.

**Progress**: 11/13 tasks closed. Axum server, rmcp MCP transport, rusqlite+WAL, refinery migrations, broadcast events, test infra all done. Remaining: Clerk JWT auth (`168f`), repository pattern enforcement (`u0rv`).

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
- AUTH-01 (Clerk JWT validation on startup/session)
- AUTH-02 (JWKS key caching)
- AUTH-03 (Clerk user ID extraction)
- AUTH-04 (desktop passes Clerk token)
- CFG-01 (settings in DB)
- CFG-02 (project registry)

**Success criteria**:
1. Server starts with a valid Clerk JWT, rejects invalid/expired tokens
2. DB created at `~/.djinn/djinn.db` with WAL mode, refinery migrations applied
3. MCP transport accepts connections and serves a `system_ping` tool
4. Repository pattern enforced: `Connection` private, all writes emit events via broadcast channel
5. Settings and projects stored in DB with CRUD operations
6. JWKS cached with 1-hour TTL; re-fetched on key rotation

**Depends on**: Nothing (first phase)

## Phase 2: Task Board

**Goal**: Full task board functionality — epics, tasks, state machine, blockers, activity log. The core domain model.

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
1. Create epics and tasks via MCP tools; tasks enforce parent epic requirement
2. Task state machine prevents illegal transitions at compile time (typestate)
3. Blocker dependencies prevent task dispatch when blockers are open
4. Short IDs generated and unique; resolvable in all task tools
5. Activity log records all state changes with structured JSON payloads
6. Board health detects stale tasks and stuck states

**Depends on**: Phase 1 (DB, MCP server, repository pattern)

---

## Phase 3: Memory / Knowledge Base

**Goal**: Full knowledge base — notes, FTS5 search, wikilink graph, memory↔task references.

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
1. Create, read, edit, search, delete notes via MCP tools
2. FTS5 search returns ranked results with snippets
3. Wikilinks resolved bidirectionally; graph endpoint returns all edges
4. Memory↔task references work in both directions (task → notes, note → tasks)
5. Catalog auto-generated from index; orphans and broken links detected

**Depends on**: Phase 2 (task board — for memory↔task references)

---

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

**Goal**: The coordinator dispatches agents to tasks, manages model health, handles graceful shutdown. The "brain" of the system.

**Requirements addressed**:
- AGENT-01 (actor hierarchy)
- AGENT-02 (three agent types)
- AGENT-03 (dispatch via summon)
- AGENT-04 (model discovery)
- AGENT-05 (model health / circuit breakers)
- AGENT-06 (session limiting)
- AGENT-07 (event-driven dispatch)
- AGENT-08 (stuck detection)
- AGENT-09 (graceful shutdown)
- AGENT-10 (WIP commits on pause)
- AGENT-11 (actor struct limits)
- AGENT-12 (scaffold system)
- CFG-04 (model configuration)

**Success criteria**:
1. Coordinator dispatches worker agent to an open task; agent works in worktree
2. Model health tracks failures; circuit breaker trips after threshold; reroutes to alternative
3. Session limiting enforces per-model capacity
4. Graceful shutdown: SIGTERM → WIP commit → agent stop → worktree preserved
5. Stuck detection recovers tasks from unresponsive agents within 30s

**Depends on**: Phase 2 (task board for dispatch), Phase 4 (git for worktrees)

---

## Phase 6: Review System

**Goal**: Task review and epic review agents verify quality before approval.

**Requirements addressed**:
- REVIEW-01 (task review: AC verification + code nitpicks)
- REVIEW-02 (epic review: completeness + aggregate quality)
- REVIEW-03 (rejection → rework loop)

**Success criteria**:
1. Task review agent checks acceptance criteria against code diff; approves or rejects with feedback
2. Epic review agent checks for missing tasks and reviews aggregate diff for patterns/duplication
3. Rejected tasks return to agent with feedback; agent reworks and resubmits
4. Full review cycle: work → task review → approve/reject → epic review → close

**Depends on**: Phase 5 (coordinator for agent dispatch), Phase 4 (git for diffs)

---

## Phase 7: Desktop Integration and Sync

**Goal**: SSE change feed, direct DB reads, task sync via git branch, server lifecycle management. Desktop can consume the full system.

**Requirements addressed**:
- MCP-04 (SSE change feed with full entities)
- MCP-03 (MCP-connect bridge mode)
- DB-05a (desktop initial load via direct DB read)
- SYNC-01 (task sync via djinn/tasks branch)
- SYNC-02 (fetch-rebase-push)
- SYNC-03 (backoff on failures)
- SYNC-04 (enable/disable per-machine)
- WSL-01 (bind 0.0.0.0)
- WSL-02 (Linux filesystem)
- WSL-03 (HTTP over TCP)
- WSL-04 (runtime detection of direct DB access)
- LIFE-01 (desktop spawns server as child process)
- LIFE-02 (graceful shutdown on SIGTERM)
- LIFE-03 (graceful restart for updates)
- LIFE-04 (board reconciliation on startup)
- LIFE-05 (desktop monitors server process)
- OBS-01 (activity in DB)
- OBS-02 (file-based operational log)

**Success criteria**:
1. Desktop connects via SSE; receives full-entity events for all mutations
2. Desktop reads DB file directly for initial load (local mode)
3. MCP-connect bridge injects project/task context into agent sessions
4. Task sync exports/imports via djinn/tasks git branch with conflict resolution
5. WSL mode: server accessible from Windows desktop via TCP
6. Graceful shutdown: SIGTERM → WIP commit → WAL checkpoint → clean exit
7. Graceful restart: desktop signals SIGTERM → waits → starts new binary → resumes from DB
8. Board reconciliation on startup detects and heals interrupted agents

**Depends on**: Phase 2 (tasks), Phase 3 (memory), Phase 5 (coordinator events)

## Phase Dependency Graph

Updated to reflect Phase 4 completed early in parallel with Phase 1.

```
Phase 1: Foundation 🟡 (85%)          Phase 4: Git ✅ (done)
    ↓                                      ↓
Phase 2: Task Board ⚪                     │
    ↓            ↘                         │
Phase 3: Memory   ↘                       │
    ↓              Phase 5: Coordinator ←──┘
    ↓                    ↓
    ↓              Phase 6: Review
    ↓                 ↙
Phase 7: Desktop Integration & Sync
```

**Next up**: Finish Phase 1 (`168f`, `u0rv`) → unblocks Phase 2 → unblocks Phases 3 and 5.

## Coverage Check

All 85 v1 requirements are assigned to exactly one phase:
- Phase 1: DB-01..07, MCP-01/02/05, AUTH-01..04, CFG-01/02 (17 reqs)
- Phase 2: TASK-01..14 (14 reqs)
- Phase 3: MEM-01..10 (10 reqs)
- Phase 4: GIT-01..08, CFG-03 (9 reqs)
- Phase 5: AGENT-01..12, CFG-04 (13 reqs)
- Phase 6: REVIEW-01..03 (3 reqs)
- Phase 7: MCP-03/04, DB-05a, SYNC-01..04, WSL-01..04, LIFE-01..05, OBS-01/02 (18 reqs)
- Cross-cutting: TEST-01..03 (3 reqs — testing patterns applied per phase)

Total: 87 (85 v1 + 2 cross-cutting TEST reqs counted separately) ✓

Note: AUTH-01..04 replace the original LIC-01..02 (ADR-004). LIFE-01..05 and TEST-01..03 are new categories added after the initial requirements pass.

## Relations

- [[Project Brief]] — vision and scope defining the roadmap
- [[V1 Requirements]] — requirements consumed by each phase
- [[Research Summary]] — synthesis informing phase sequencing
- [[Database Layer — rusqlite over libsql/Turso]] — ADR-002 driving DB and desktop integration phases
- [[Migrations — refinery with timestamp-based naming]] — ADR-003 driving migration approach in Phase 1
- [[Authentication — Clerk JWT Validation]] — ADR-004 driving auth in Phase 1 (replaces LIC)
- [[Server Lifecycle — Desktop-Managed Daemon with Graceful Restart]] — ADR-005 driving lifecycle in Phase 7

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
| AUTH-* | ADR-004 (Clerk JWT Validation) |
| LIFE-* | ADR-005 (Server Lifecycle) |
| TEST-* | Research Summary, Stack Research |
| CFG-* | Brief (scope section) |
| WSL-* | Architecture Research §7 (WSL considerations), ADR-002 |
