---
title: Roadmap
type: roadmap
tags: []
---





# Roadmap — Djinn Server Rust Rewrite

Phased delivery plan for v1 requirements. Each phase builds on the previous and has testable success criteria. Phases are sequenced by real dependencies — later phases require earlier foundations.

## Progress Overview

_Updated: 2026-03-06_

| Phase | Status | Remaining |
|-------|--------|-----------|
| Phase 1: Foundation | Complete | -- |
| Phase 2: Task Board | Complete | -- |
| Phase 3: Knowledge Base | Complete | -- |
| Phase 4: Git Integration | Complete | -- |
| Phase 5: Coordinator | Complete | -- |
| Phase 6: Review | Complete | -- |
| Phase 7: Desktop & Sync | Complete | -- |
| Phase 8: Session Visibility | Complete | -- |
| Phase 9: V1 Completion | Complete | `ewbt` (KB file watcher only) |
| Phase 10: Operational Reliability | Not started | ADR-022: outcome-based validation |
| Phase 11: Cognitive Memory | Not started | ADR-023: multi-signal retrieval, associations, confidence |

**All V1 server phases complete (55/55 items, 100%).**
**Phase 10 addresses operational reliability: outcome-based worker validation, AC-driven reviewer verdicts, circuit breakers. See [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]].**
**Phase 11 upgrades the KB to a cognitive memory system for multi-agent scale: RRF search, Hebbian associations, Bayesian confidence, contradiction detection, context compression. See [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]].**

**ADR-008:** Goose library replaces summon. MCP-connect bridge (`1tst`) and scaffold system (`1nby`) dropped. See [[ADR-008: Agent Harness -- Goose Library over Summon Subprocess Spawning]].

**ADR-009:** Phases eliminated. No dispatch grouping -- tasks dispatch when open + unblocked. Simplified execution tools (6 instead of 26). See [[ADR-009: Simplified Execution -- No Phases, Direct Task Dispatch]].

**ADR-010:** Session cost tracking. Per-task session history with token metrics for desktop visibility. See [[ADR-010: Session Cost Tracking -- Per-Task Token Metrics]].

**ADR-012:** Epic review batches. Tasks close immediately after merge; epic review runs as persisted batch orchestration. Structured output nudging with retry budget. See [[ADR-012 Epic Review Batches and Structured Output Nudging]].

**ADR-013:** OS-level shell sandboxing. Landlock (Linux) + Seatbelt (macOS) for kernel-enforced filesystem isolation. Supersedes ADR-011. See [[ADR-013: OS-Level Shell Sandboxing -- Landlock + Seatbelt]].

**ADR-022:** Outcome-based session validation. Git diff replaces worker DONE marker; AC met state replaces reviewer text markers; circuit breakers prevent infinite loops. See [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]].

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
- GIT-01 extension (best-effort rebase of reused task branches before dispatch)
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
4. ~~Graceful shutdown: CancellationToken → Goose agent stops → WIP commit/capture and worktree cleanup handled by supervisor~~ ✓
5. ~~Stuck detection recovers tasks from unresponsive agents within 30s~~ ✓
6. ~~Credential vault stores API keys; Goose providers created from vault at dispatch time~~ ✓
7. ~~Per-session prompt and extension configuration for different agent types~~ ✓
8. OAuth-capable Goose providers are discoverable/configurable over MCP and can dispatch without manual API key entry

**Depends on**: Phase 2 (task board for dispatch), Phase 4 (git for worktrees)


## Phase 6: Review System

**Goal**: Task review and epic review agents verify quality before approval. **Runs as Goose sessions per ADR-008.**

**Progress**: COMPLETE. Review agents (`lm7a`) closed. Scaffold system (`1nby`) dropped per ADR-008. Coordinator dispatches task-review agents for tasks in `needs_task_review`; epic review now dispatches from persisted epic review batches when epics move to `in_review`. Supervisor handles all three agent types (worker, task_reviewer, epic_reviewer).

**Requirements addressed**:
- REVIEW-01 (task review: AC verification + code nitpicks)
- REVIEW-02 (epic review: completeness + aggregate quality)
- REVIEW-03 (rejection → rework loop)

**Success criteria**:
1. ~~Task review Goose agent checks acceptance criteria against code diff; approves or rejects with feedback~~ ✓
2. ~~Epic review Goose agent reviews aggregate diff for patterns/duplication~~ ✓
3. ~~Rejected tasks return to agent with feedback; agent reworks and resubmits~~ ✓
4. ~~Full review cycle: work → task review → epic review → approve/reject → close~~ ✓

**Depends on**: Phase 5 (coordinator for agent dispatch), Phase 4 (git for diffs)


## Phase 7: Desktop Integration and Sync ✅

**Goal**: SSE change feed, direct DB reads, task sync via git branch, server lifecycle management. Desktop can consume the full system.

**Progress**: COMPLETE. SSE change feed (`ywb0`), djinn/ namespace sync (`2up9`), server lifecycle (`qhb4`) all implemented. MCP-connect bridge (`1tst`) dropped per ADR-008. Daemon mode with `daemon.json` discovery, graceful shutdown, `--mcp-connect` stdio bridge, `--ensure-daemon`, settings tools with file watcher.

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
4. ~~Server lifecycle: desktop-spawned daemon, standalone VPS mode, graceful restart, board reconciliation~~ ✓
5. ~~daemon.json discovery file written on startup with pid/port/started_at~~ ✓

**Depends on**: Phase 2 (tasks), Phase 3 (memory), Phase 5 (coordinator events)


## Phase 8: Session Visibility & Cost Tracking ✅

**Goal**: First-class session tracking with real-time desktop visibility and token/cost metrics. Sessions are viewable entities — users see active sessions in real-time, browse session history per task, and track cost across runs. **See ADR-010.**

**Progress**: COMPLETE. All 3 features implemented: Session schema + repository (`yvw6`), token metrics capture with dual-path fallback (`1x2s`), session MCP tools + task_show enrichment (`32j6`). Stale session detection and recovery included.

**Requirements addressed**:
- AGENT-19 (NEW: session persistence with token metrics)
- OBS-01 (extends: session events in activity stream)
- AGENT-15 foundation (v2: compute governance / ACU budgets)

**Success criteria**:
1. ~~Sessions table tracks every agent session with task_id, model_id, agent_type, tokens_in/out~~ ✓
2. ~~Supervisor writes session records on dispatch start and completion~~ ✓
3. ~~SSE events emitted for session lifecycle (started, completed, interrupted, failed)~~ ✓
4. ~~task_show includes active session and session count~~ ✓
5. ~~session_list/session_active MCP tools return session data for desktop~~ ✓

**Depends on**: Phase 5 (supervisor writes sessions)

## Phase 9: V1 Completion — Audit Gaps ✅

**Goal**: Close gaps found in the Go server comparison audit. Covers execution control, project tools, operational logging, conflict resolution, structured output parsing, merge tracking, and file watchers. **See ADR-009 for simplified execution model. ADR-012 for epic review batches and output nudging.**

**Progress**: COMPLETE (7.5/8). All items implemented except KB file watcher (settings file watcher done). Epic review batches landed per ADR-012 — tasks close immediately, batch orchestration for epic review, structured output nudging with 2-retry budget.

**Features/Tasks**:
- `cu4v` — ~~Simplified execution control MCP tools~~ ✓ — start/pause/resume/status/kill + session_for_task
- `1upo` — ~~Project management MCP tools~~ ✓ — project_add/list/remove with validation
- `18a0` — ~~Operational logging with file rotation~~ ✓ — tracing-appender daily rotation + 7-day retention + system_logs tool
- `layi` — ~~Conflict resolution merge flow~~ ✓ — conflict detection, ConflictResolver agent type, prompt template
- `lypu` — ~~Structured agent output parsing~~ ✓ — WORKER_RESULT/REVIEW_RESULT/EPIC_REVIEW_RESULT + nudging per ADR-012
- `1i5q` — ~~Store merge_commit_sha on task~~ ✓ — field on Task model, persisted after squash-merge
- `ewbt` — File watchers — ⚠️ PARTIAL: settings file watcher done (notify crate, debounce), KB file watcher not yet implemented
- `stdio-bridge` — ~~`djinn-server --mcp-connect` stdio↔HTTP MCP bridge~~ ✓ — full forwarding to daemon HTTP endpoint

**Requirements addressed**:
- GIT-09 (merge_commit_sha on task)
- OBS-02 (file-based operational log)
- CFG-02 (project registry MCP tools — tools were missing, repo existed)
- REVIEW-01/02/03 (structured output parsing completes review flow)

**Success criteria**:
1. ~~Desktop can start/pause/resume/kill execution via 6 MCP tools~~ ✓
2. ~~Desktop can manage projects via 3 MCP tools~~ ✓
3. ~~Logs written to ~/.djinn/logs/ with rotation; accessible via system_logs tool~~ ✓
4. ~~Merge conflicts detected and resolved via agent rework loop~~ ✓
5. ~~Agent verdicts parsed from output stream and drive state transitions~~ ✓
6. ~~merge_commit_sha stored on task after successful squash-merge~~ ✓
7. External KB edits detected and re-indexed automatically — ⚠️ settings only, KB pending

**Depends on**: Phase 5 (coordinator/supervisor for execution tools), Phase 4 (git for conflict resolution)

## Phase Dependency Graph

```
Phase 1-9: V1 Complete
    |
Phase 10: Operational Reliability (ADR-022)
    |
Phase 11: Cognitive Memory Infrastructure (ADR-023)

Phase 12: Own the Agent Loop (ADR-027) — can run in parallel with 10/11
```

All V1 server phases complete. Phase 10 addresses operational reliability. Phase 11 upgrades the memory system for multi-agent scale. Phase 12 replaces Goose with a Djinn-owned agent loop.

## Phase 12: Own the Agent Loop — Replace Goose with Direct LLM Integration

**Goal**: Remove the Goose library dependency entirely. Djinn owns the full agent loop: LLM API calls, SSE streaming, tool dispatch, compaction, OAuth, session storage, and observability. **See [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]].**

**Progress**: Not started.

**Requirements addressed**:
- AGENT-03 (revised: direct LLM API calls replace Goose library)
- AGENT-17 (revised: provider creation from vault without Goose)
- AGENT-18 (revised: per-session config without Goose Agent)
- AGENT-19 (revised: session messages in Djinn's single DB)
- NEW OBS-03: Langfuse LLM observability (traces + generations)

**Features** (to be broken down by `/breakdown`):
- Provider HTTP layer with 3 format families (OpenAI-compatible, Anthropic, Google)
- Reply loop (stream → tool dispatch → continue)
- Compaction (copied from Goose as-is initially)
- OAuth flows (ChatGPT Codex PKCE, GitHub Copilot device code)
- Session message consolidation into Djinn's DB
- Developer tools port (write/edit)
- Message types (Djinn-native)
- Token counting (API response primary, tiktoken fallback)
- Langfuse observability client
- Goose crate removal

**Success criteria**:
1. Agent sessions run without any Goose crate dependency
2. All 3 format families stream LLM responses and handle tool calls correctly
3. Codex OAuth flow authenticates and dispatches agents against ChatGPT subscription
4. Copilot OAuth flow authenticates and dispatches agents against GitHub Copilot
5. Session conversation history stored in Djinn's main DB (no separate sessions.db)
6. Compaction fires at 80% context usage and agent continues in fresh context
7. Langfuse receives traces with token counts for every LLM generation
8. All existing tests pass with Goose removed
9. `~/.djinn/sessions/` directory no longer created

**Depends on**: Phase 9 (V1 complete). Can run in parallel with Phases 10 and 11.

**Estimation**: ~3000-3500 LOC new/adapted code. See [[Agent Loop Port Scope]].

## Phase 10: Operational Reliability — Outcome-Based Validation & Agent Roles

**Goal**: Replace unreliable text-marker-based session routing with outcome-based validation. Workers validated by git diff, reviewers validated by AC state, circuit breakers prevent infinite loops. **See [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]].**

**Progress**: Not started.

**Requirements addressed**:
- REVIEW-01 extension (AC-driven reviewer verdicts replace text markers)
- REVIEW-03 extension (circuit breaker on reopen limit)
- AGENT-08 extension (outcome-based stuck detection replaces marker-based)
- NEW AGENT-20: Git diff as worker completion signal
- NEW AGENT-21: Evidence-based nudging with retry budget
- NEW AGENT-22: Task-level circuit breaker (no-changes, reopen limit, session errors)
- NEW REVIEW-04: AC-only reviewer verdicts (workers cannot update AC met status)

**Features/Tasks** (to be broken down):
- Outcome-based worker validation — git diff check after reply loop, NO_CHANGES_NEEDED signal
- AC-driven reviewer verdicts — derive VERIFIED/REOPEN from AC met state, not text markers
- Evidence-based nudging — git diff evidence in nudge, max 2 attempts
- Task-level circuit breaker — fail after no-changes/reopen-limit/session-error thresholds
- Worker AC restriction — prevent workers from updating AC met status
- Write-tool tracking — distinguish "explored but didn't implement" from "genuinely done"

**Success criteria**:
1. Worker that produces file changes proceeds to review without needing a text marker
2. Worker that produces no changes gets evidence-based nudge showing empty git diff
3. Worker that produces no changes after 2 nudges has task marked failed
4. Task reopened 3+ times by reviewer is marked failed for human triage
5. Reviewer verdict derived from AC met/unmet state, not from REVIEW_RESULT text
6. Workers cannot call task_update to set acceptance_criteria met status
7. NO_CHANGES_NEEDED signal passes to reviewer who independently verifies the claim

**Depends on**: Phase 9 (V1 complete), Phase 6 (review system)

**ADR-024:** Agent role redesign. EpicReviewer killed, replaced by PM (backlog grooming, circuit breaker escalation, KB hygiene) and Architect (codebase analysis, ADR enforcement, proposals). ADR status gains system semantics (Proposed/Accepted/Superseded/Rejected). Workers lose `task_update`. See [[ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline]].

**ADR-025:** Backlog grooming and dispatch triggers. `Draft` renamed to `Backlog` as default status. PM triggered by debounced backlog watch. Architect triggered by merge count threshold. PM has dispatch priority over workers. See [[ADR-025: Backlog Grooming and Autonomous Dispatch Triggers]].


## Phase 11: Cognitive Memory Infrastructure

**Goal**: Upgrade the knowledge base from a static note store with FTS search to a cognitive memory system with multi-signal retrieval, implicit association learning, confidence scoring, and context compression. Designed for multi-agent scale (hundreds of concurrent agents, thousands of tasks). **See [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]].**

**Progress**: Not started.

**Requirements addressed**:
- CMEM-01 (multi-signal RRF search)
- CMEM-02 (ACT-R temporal priority)
- CMEM-03 (access frequency tracking)
- CMEM-04 (graph proximity scoring)
- CMEM-05 (task affinity scoring)
- CMEM-06 (Hebbian association learning)
- CMEM-07 (Bayesian confidence scoring)
- CMEM-08 (contradiction detection)
- CMEM-09 (context compression / progressive disclosure)
- CMEM-10 (note summaries)
- CMEM-11 (session reflection)
- CMEM-12 (association pruning)
- CMEM-13 (FTS5 field weighting)
- CMEM-14 (memory domain scoping)

**Sub-phases**:

### 11a: Retrieval Pipeline
- Schema migration: `access_count`, `confidence`, `summary` columns on notes
- FTS5 field weighting (title=3×, tags=2×, content=1×)
- ACT-R temporal priority function (query-time computation)
- Graph proximity scoring (BFS + 0.7× hop decay)
- Task affinity scoring (memory_refs on related tasks)
- RRF fusion of 4 signals with configurable k-constants
- `build_context` upgrade with progressive disclosure

### 11b: Association Learning
- `note_associations` table schema + migration
- Co-access tracking (session-scoped batches)
- Hebbian weight updates on session completion
- Association pruning (periodic, low-weight cleanup)
- Implicit associations as graph proximity signal
- `memory_associations` MCP tool

### 11c: Confidence & Contradiction
- Bayesian confidence update function
- Task outcome → confidence signal (success/failure)
- Concept-cluster contradiction detection on write
- Contradiction event emission
- Confidence in search results and note reads

### 11d: Session Reflection
- Post-task reflection job in supervisor
- Co-access extraction from session tool log
- Batch Hebbian + confidence updates
- Access count bulk update

**Success criteria**:
1. `memory_search` returns results ranked by RRF-fused score (BM25 + temporal + graph + task affinity)
2. Notes accessed frequently and recently rank measurably higher than equivalent stale notes
3. Notes co-accessed by 10+ agent sessions show implicit associations without manual wikilinks
4. Task completion updates confidence scores on referenced notes
5. Writing a note with high FTS overlap against an existing note flags a potential contradiction
6. `build_context` returns top-K related notes as summaries, not full content
7. Post-session reflection updates association weights and confidence for notes accessed during the session

**Depends on**: Phase 9 (V1 complete — existing KB infrastructure), Phase 10 (operational reliability — session outcome tracking provides confidence signals)

**Research**: [[Cognitive Memory Systems Research]] — comparative analysis of MuninnDB, Augment Code, Letta/MemGPT, GitHub Copilot, Cognee, and git-based context patterns.

## Phase Dependency Graph

```
Phase 1-4: Foundation, Task Board, KB, Git
    |
Phase 5: Coordinator
    |
Phase 6: Review
    |
Phase 7: Desktop & Sync
    |
Phase 8: Session Visibility
    |
Phase 9: V1 Completion (KB file watcher pending)
    |
Phase 10: Operational Reliability (ADR-022)
    |
Phase 11: Cognitive Memory Infrastructure (ADR-023)
```

All V1 server phases complete. Phase 10 addresses operational reliability. Phase 11 upgrades the memory system for multi-agent scale.

## Coverage Check

Updated 2026-03-04. All phases complete. ADR-012 adds epic review batches. ADR-013 adds OS-level sandboxing (future work, not a V1 requirement).

- Phase 1: DB-01..07, MCP-01/02/05, CFG-01/02 (13 reqs) ✅
- Phase 2: TASK-01..14 (14 reqs) ✅
- Phase 3: MEM-01..10 (10 reqs) ✅
- Phase 4: GIT-01..08, CFG-03 (9 reqs) ✅
- Phase 5: AGENT-01..11, AGENT-16..18, CFG-04 (15 reqs) ✅
- Phase 6: REVIEW-01..03 (3 reqs) ✅
- Phase 7: MCP-04, DB-05a, SYNC-01..04, WSL-01..04, LIFE-01..05 (16 reqs) ✅
- Phase 8: AGENT-19, OBS-01 extension (2 reqs) ✅
- Phase 9: GIT-09, OBS-02, CFG-02 MCP tools, REVIEW-01..03 completion (5 reqs) ✅
- Cross-cutting: TEST-01..03 (3 reqs) — Phase 1 complete (ADR-026, epic `b11g` closed 2026-03-12; 454 tests, all passing)

- Phase 10: AGENT-20..22, REVIEW-04 (4 reqs)
- Phase 11: CMEM-01..14 (14 reqs)

Total: 112 (94 prior + 4 Phase 10 + 14 Phase 11) ✓


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
- [[ADR-012 Epic Review Batches and Structured Output Nudging]] — ADR-012 driving epic review batch orchestration and output nudging
- [[ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt]] — ADR-013 for future OS-level agent sandboxing


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