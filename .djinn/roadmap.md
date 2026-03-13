---
title: Roadmap
type: roadmap
tags: []
---






# Roadmap — Djinn Server Rust Rewrite

Phased delivery plan for v1 requirements. Each phase builds on the previous and has testable success criteria. Phases are sequenced by real dependencies — later phases require earlier foundations.

## Progress Overview

_Updated: 2026-03-13_

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
| Phase 9: V1 Completion | Complete | -- |
| Phase 10: Architecture & Agent Roles | Complete | -- |
| Phase 11: Own the Agent Loop | Complete | -- |
| Phase 12: Backlog Grooming | Nearly complete | 1 task (`78gy` SSE event) |
| Phase 13: Chat Experience | In progress | 3 tasks remaining |
| Phase 14: Desktop SSE Completeness | In progress | 5 tasks remaining |
| Phase 15: Deep Module Architecture | In progress | 2 tasks remaining |
| Phase 16: Operational Reliability | Not started | ADR-022 remaining items |
| Phase 17: Cognitive Memory | Not started | ADR-023: full scope |
| Phase 18: Test Coverage & CI | Not started | ADR-026 Phases 2-3 |

**V1 server phases 1-9 complete (55/55 items, 100%).**
**Post-V1 phases 10-11 complete: slot architecture, PM intervention, agent loop ownership.**
**Phases 12-15 in progress: grooming, chat, SSE completeness, deep modules.**
**Phases 16-18 planned: operational reliability, cognitive memory, test/CI pipeline.**

**ADR-008:** Goose library replaced summon — then itself replaced by own agent loop (ADR-027). MCP-connect bridge (`1tst`) and scaffold system (`1nby`) dropped. See [[ADR-008: Agent Harness -- Goose Library over Summon Subprocess Spawning]].

**ADR-009:** Phases eliminated. No dispatch grouping -- tasks dispatch when open + unblocked. Simplified execution tools (6 instead of 26). See [[ADR-009: Simplified Execution -- No Phases, Direct Task Dispatch]].

**ADR-010:** Session cost tracking. Per-task session history with token metrics for desktop visibility. See [[ADR-010: Session Cost Tracking -- Per-Task Token Metrics]].

**ADR-012:** Epic review batches. Tasks close immediately after merge; epic review runs as persisted batch orchestration. Structured output nudging with retry budget. See [[ADR-012 Epic Review Batches and Structured Output Nudging]].

**ADR-013:** OS-level shell sandboxing. Landlock (Linux) + Seatbelt (macOS) for kernel-enforced filesystem isolation. Supersedes ADR-011. See [[ADR-013: OS-Level Shell Sandboxing -- Landlock + Seatbelt]].

**ADR-022:** Outcome-based session validation. Git diff replaces worker DONE marker; AC met state replaces reviewer text markers; circuit breakers prevent infinite loops. See [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]].

**ADR-027:** Own the agent loop. Goose fully replaced with Djinn-owned provider abstraction, reply loop, compaction, OAuth, session messages, and Langfuse telemetry. See [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]].

**ADR-028:** Deep module architecture. `#![warn(unreachable_pub)]` enforced, facade re-exports, pub(crate) sweep, cross-coupling extraction. See [[ADR-028: Module Visibility Enforcement and Deep Module Architecture]].

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

**Progress**: COMPLETE (8/8). All items implemented including KB file watcher. Epic review batches landed per ADR-012 — tasks close immediately, batch orchestration for epic review, structured output nudging with 2-retry budget.

**Features/Tasks**:
- `cu4v` — ~~Simplified execution control MCP tools~~ ✓ — start/pause/resume/status/kill + session_for_task
- `1upo` — ~~Project management MCP tools~~ ✓ — project_add/list/remove with validation
- `18a0` — ~~Operational logging with file rotation~~ ✓ — tracing-appender daily rotation + 7-day retention + system_logs tool
- `layi` — ~~Conflict resolution merge flow~~ ✓ — conflict detection, ConflictResolver agent type, prompt template
- `lypu` — ~~Structured agent output parsing~~ ✓ — WORKER_RESULT/REVIEW_RESULT/EPIC_REVIEW_RESULT + nudging per ADR-012
- `1i5q` — ~~Store merge_commit_sha on task~~ ✓ — field on Task model, persisted after squash-merge
- `ewbt` — ~~File watchers~~ ✓ — settings file watcher (notify crate, debounce) + KB note file watcher (watches .djinn/ per project, reindex_from_disk on .md changes)
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
7. ~~External KB edits detected and re-indexed automatically~~ ✓

**Depends on**: Phase 5 (coordinator/supervisor for execution tools), Phase 4 (git for conflict resolution)

---

## Post-V1 Phases

## Phase 10: Architecture & Agent Roles ✅

**Goal**: Replace the monolithic AgentSupervisor with slot-based parallelism, add PM agent for circuit-breaker escalation, per-project configuration MCP tools, and session continuation. Covers ADR-014, ADR-015, ADR-024 (PM only), and the slot architecture.

**Progress**: COMPLETE. Slot-based supervisor (epic `zlrv`, 7/7 tasks), PM intervention (migrations V20260309000003-4, AgentType::PM, coordinator dispatch, prompt), project configuration MCP (epic `lirr`, 2/2 tasks), session continuation (epic `r7ez`), OS shell hardening (epic `0qu2`), session compaction (epic `aj6b`).

**Epics**: `zlrv` (Slot-Based Supervisor), `lirr` (Project Config MCP), `r7ez` (Setup & Session Resume), `0qu2` (OS Shell Hardening), `aj6b` (Session Compaction)

**Key deliverables**:
- SlotPool architecture: `src/actors/slot/` module — lifecycle, reply_loop, helpers, worktree, commands, task_review, pool/
- PM agent: `AgentType::PM`, `needs_pm_intervention`/`in_pm_intervention` statuses, PM prompt, coordinator dispatch
- Circuit breaker: `continuation_count` ≥ 3 stale cycles → `Escalate` → PM intervention
- Project config: `project_config_get`/`project_config_set` MCP tools, per-project target_branch/auto_merge/sync settings
- Session continuation: worker pauses (not completes), reviewer feedback appended, conversation resumed
- Compaction: 80% context threshold, LLM summarization, continuation_of chain
- OS sandboxing: Landlock (Linux) + Seatbelt (macOS) kernel-enforced filesystem isolation

**Success criteria**:
1. ~~Slots own full task lifecycle; parallel post-session processing~~ ✓
2. ~~PM agent dispatched for tasks stuck ≥3 stale review cycles~~ ✓
3. ~~Per-project config persisted and accessible via MCP~~ ✓
4. ~~Session continuation resumes conversation with reviewer feedback~~ ✓
5. ~~Compaction fires at 80% context, agent continues in fresh context~~ ✓

**Depends on**: Phase 9 (V1 complete)

## Phase 11: Own the Agent Loop ✅

**Goal**: Remove the Goose library dependency entirely. Djinn owns the full agent loop: LLM API calls, SSE streaming, tool dispatch, compaction, OAuth, session storage, and observability. **See [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]].**

**Progress**: COMPLETE. Epic `q81u` — all 9/9 tasks closed: native message types (`lctb`), provider HTTP layer (`8o1w`), developer tools port (`dsb7`), session message storage (`a87g`), reply loop (`ty9u`), compaction (`zih5`), OAuth flows (`sbue`), lifecycle rewiring (`g7qy`), Goose crate removal (`qmcl`).

**Epic**: `q81u` (Own the Agent Loop — Replace Goose)

**Requirements addressed**:
- AGENT-03 (revised: direct LLM API calls replace Goose library)
- AGENT-17 (revised: provider creation from vault without Goose)
- AGENT-18 (revised: per-session config without Goose Agent)
- AGENT-19 (revised: session messages in Djinn's single DB)
- OBS-03: Langfuse/OpenTelemetry LLM observability (traces + generations)

**Key deliverables**:
- 4 provider format families: OpenAI-compatible, OpenAI Responses (Codex), Anthropic, Google — in `src/agent/provider/format/`
- `ApiClient` with SSE streaming, exponential backoff retry, 600s timeout
- `run_reply_loop()` — stream consumption, tool dispatch, token tracking, context-length compaction
- Djinn-native `Message`/`ContentBlock`/`Conversation` types with per-format wire serializers
- `SessionMessageRepository` — messages in single DB, batch insert, conversation load
- OAuth: GitHub Copilot device code flow + ChatGPT Codex PKCE flow in `src/agent/oauth/`
- Langfuse telemetry via OpenTelemetry OTLP export — session/LLM/tool span hierarchy
- Token counting from provider API responses (no tiktoken needed)
- Zero Goose dependency in Cargo.toml

**Success criteria**:
1. ~~Agent sessions run without any Goose crate dependency~~ ✓
2. ~~All 4 format families stream LLM responses and handle tool calls correctly~~ ✓
3. ~~Codex OAuth flow authenticates and dispatches agents~~ ✓
4. ~~Copilot OAuth flow authenticates and dispatches agents~~ ✓
5. ~~Session conversation history stored in Djinn's main DB~~ ✓
6. ~~Compaction fires at 80% context usage and agent continues~~ ✓
7. ~~Langfuse receives traces with token counts~~ ✓
8. ~~All existing tests pass with Goose removed~~ ✓

**Depends on**: Phase 9 (V1 complete). Ran in parallel with Phase 10.

## Phase 12: Backlog Grooming — ADR-025

**Goal**: Every task passes through a grooming quality gate before worker dispatch. Groomer agent validates AC, scope, design, and memory refs. **See [[ADR-025: Backlog Grooming and Autonomous Dispatch Triggers]].**

**Progress**: Nearly complete (18/19 tasks). Epic `rewx`. Groomer agent type, prompt, coordinator debounced backlog watcher, project-scoped session support, nullable session task_id all implemented. 1 SSE event task remaining.

**Epic**: `rewx` (Backlog Grooming — ADR-025)

**Key deliverables**:
- `AgentType::Groomer` with project-scoped lifecycle (no worktree, uses project_dir)
- Groomer prompt: `src/agent/prompts/groomer.md` — validates AC, scope, design, memory refs; promotes or improves
- `dispatch_groomer_for_project()` in coordinator — debounced backlog monitoring, single groomer per project
- `run_project_lifecycle()` in slot lifecycle — handles non-worktree agent sessions
- `Backlog` as default task status (renamed from Draft)

**Remaining**:
- `78gy` — Emit SessionDispatched SSE event for project-scoped sessions (P2, 2 verification failures)

**Success criteria**:
1. ~~Groomer dispatched when backlog tasks exist~~ ✓
2. ~~Quality gate enforces AC, scope, design on every task~~ ✓
3. ~~Project-scoped sessions work without worktree~~ ✓
4. SessionDispatched SSE emitted for project-scoped sessions — pending `78gy`

**Depends on**: Phase 10 (slot architecture, PM agent), Phase 11 (own agent loop)

## Phase 13: Chat Experience

**Goal**: Server-side streaming chat endpoint enabling the desktop chat UI to interact with an LLM that has MCP tool access for project management through conversation.

**Progress**: In progress (5/8 tasks). Epic `xo4q`. Streaming endpoint, system prompt, project context injection, initial tool schemas, and MCP dispatch bridge all implemented. Remaining work: unify chat tool schemas with MCP router, wire full dispatch, and add integration tests.

**Epic**: `xo4q` (Chat Experience)

**Key deliverables**:
- `POST /api/chat/completions` streaming endpoint in `src/server/chat.rs`
- SSE response format: delta, tool_call, tool_result, done, error events
- Chat system prompt (`src/agent/prompts/chat.md`) with workflow guidance
- Project context injection: epic/task counts, project brief in system prompt
- Up to 20 tool iterations per conversation
- Provider credential resolution and format family detection

**Remaining**:
- `zq9z` — Replace chat_tool_schemas with full MCP tool list (P0, 2 verification failures)
- `4e4q` — Wire chat handler to use MCP dispatch instead of dispatch_tool_call (P1, blocked by zq9z)
- `3w5h` — Chat integration tests — tool dispatch and system prompt (P2, blocked by 4e4q)

**Success criteria**:
1. ~~Streaming chat completions endpoint functional~~ ✓
2. ~~System prompt includes project context~~ ✓
3. Chat exposes all MCP tools (not manual subset) — pending `zq9z`
4. Tool dispatch routes through MCP server — pending `4e4q`
5. Integration tests cover dispatch routing and prompt composition — pending `3w5h`

**Depends on**: Phase 11 (own agent loop — provider abstraction), Phase 1 (MCP server)

## Phase 14: Desktop SSE Completeness

**Goal**: Ensure every repository write emits an SSE event so the desktop UI stays in sync. Close gaps in session event payloads and add missing event types.

**Progress**: In progress (8/13 tasks). Epic `br1h`. Session messages MCP tool, SessionMessage SSE event, structured commands_run events, per-command duration, interrupt_all_running fix all done. Remaining: repo-emits-on-write gaps and session payload enrichment.

**Epic**: `br1h` (Session Content & Activity Enrichment)

**Key deliverables (done)**:
- `session_messages` MCP tool for historical conversation content
- `SessionMessage` SSE event for live session streaming
- Structured `commands_run` activity events (ADR-020) with per-command duration
- `SessionUpdated` emission for `interrupt_all_running`

**Remaining**:
- `6is6` — Add project_id to session SSE event payloads (P2, 2 verification failures)
- `a5qz` — Emit TaskUpdated after blocker add/remove (P2, 1 verification failure)
- `1hdw` — Add events field to CustomProviderRepository and emit on writes (P2, needs_pm_intervention)
- `y62p` — Emit TaskUpdated from increment_continuation_count (P4)
- `9x4u` — Add ActivityLogged event and emit from log_activity (P4, 2 verification failures)

**Success criteria**:
1. ~~Session messages accessible via MCP tool and live SSE~~ ✓
2. ~~Structured activity events with duration~~ ✓
3. All repository writes emit SSE events — pending 5 tasks
4. Session lifecycle events include project_id — pending `6is6`

**Depends on**: Phase 8 (session visibility), Phase 10 (slot architecture)

## Phase 15: Deep Module Architecture — ADR-028

**Goal**: Enforce deep module pattern across db/, models/, agent/ — flatten import paths, restrict visibility at compile time, break cross-coupling. **See [[ADR-028: Module Visibility Enforcement and Deep Module Architecture]].**

**Progress**: In progress (10/12 tasks). Epic `ag0y`. `#![warn(unreachable_pub)]` enforced in lib.rs, facade re-exports on db/ and models/, exhaustive state machine tests, AC enforcement tests, security tests, MCP behavior assertions, lifecycle tests, provider validation tests, first decoupling extraction all done. 2 tasks remaining.

**Epic**: `ag0y` (Deep Module Architecture — ADR-028)

**Key deliverables (done)**:
- `#![warn(unreachable_pub)]` in `src/lib.rs`
- Facade re-exports on db/ and models/ (`er9m`)
- Exhaustive task state machine transition tests (`1f7z`)
- MCP contract tests with DB-state behavior assertions (`1xdt`)
- Security tests for extension.rs path safety and tool authorization (`4ulk`)
- Task review pipeline unit tests (`ripe`)
- Lifecycle and provider validation tests (`8x0j`, `okck`)

**Remaining**:
- `t16l` — pub(crate) sweep on agent/ internals (needs_pm_intervention, all AC met but verification failures)
- `tt9l` — Extract shared task transition types to break agent↔actor coupling (P3, blocked by t16l)

**Success criteria**:
1. ~~`#![warn(unreachable_pub)]` catches leaks at compile time~~ ✓
2. ~~Facade re-exports flatten import paths~~ ✓
3. ~~Exhaustive state machine tests~~ ✓
4. agent/ internals marked pub(crate) — pending `t16l`
5. agent↔actor cross-coupling extracted — pending `tt9l`

**Depends on**: Phase 9 (V1 complete — codebase to refactor)

## Phase 16: Operational Reliability — ADR-022

**Goal**: Replace text-marker-based session routing with outcome-based validation. Workers validated by git diff, reviewers validated by AC state. **See [[ADR-022: Outcome-Based Session Validation & Agent Role Redesign]].**

**Progress**: Not started. Circuit breaker and PM escalation already landed in Phase 10; this phase covers the remaining ADR-022 mechanism changes.

**Note**: ADR-024 PM agent and ADR-025 backlog grooming were originally scoped under this phase but delivered independently in Phases 10 and 12. What remains is the core outcome-based validation mechanism.

**Requirements addressed**:
- AGENT-20: Git diff as worker completion signal
- AGENT-21: Evidence-based nudging with retry budget
- AGENT-22: Task-level circuit breaker refinement (no-changes, session errors)
- REVIEW-04: AC-only reviewer verdicts (workers cannot update AC met status)

**Features** (to be broken down):
- Outcome-based worker validation — git diff check after reply loop, NO_CHANGES_NEEDED signal
- AC-driven reviewer verdicts — derive VERIFIED/REOPEN from AC met state, not text markers
- Evidence-based nudging — git diff evidence in nudge, max 2 attempts
- Worker AC restriction — prevent workers from updating AC met status
- Write-tool tracking — distinguish "explored but didn't implement" from "genuinely done"

**Success criteria**:
1. Worker that produces file changes proceeds to review without needing a text marker
2. Worker that produces no changes gets evidence-based nudge showing empty git diff
3. Worker that produces no changes after 2 nudges has task marked failed
4. Reviewer verdict derived from AC met/unmet state, not from REVIEW_RESULT text
5. Workers cannot call task_update to set acceptance_criteria met status

**Depends on**: Phase 11 (own agent loop — reply loop is where validation happens), Phase 10 (PM/circuit breaker foundation)

## Phase 17: Cognitive Memory Infrastructure — ADR-023

**Goal**: Upgrade the knowledge base from a static note store with FTS search to a cognitive memory system with multi-signal retrieval, implicit association learning, confidence scoring, and context compression. Designed for multi-agent scale. **See [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]].**

**Progress**: Not started. Planning complete — scope, requirements (CMEM-01 through CMEM-14), and sub-phases defined.

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

### 17a: Retrieval Pipeline
- Schema migration: `access_count`, `confidence`, `summary` columns on notes
- FTS5 field weighting (title=3×, tags=2×, content=1×)
- ACT-R temporal priority function (query-time computation)
- Graph proximity scoring (BFS + 0.7× hop decay)
- Task affinity scoring (memory_refs on related tasks)
- RRF fusion of 4 signals with configurable k-constants
- `build_context` upgrade with progressive disclosure

### 17b: Association Learning
- `note_associations` table schema + migration
- Co-access tracking (session-scoped batches)
- Hebbian weight updates on session completion
- Association pruning (periodic, low-weight cleanup)
- Implicit associations as graph proximity signal
- `memory_associations` MCP tool

### 17c: Confidence & Contradiction
- Bayesian confidence update function
- Task outcome → confidence signal (success/failure)
- Concept-cluster contradiction detection on write
- Contradiction event emission
- Confidence in search results and note reads

### 17d: Session Reflection
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

**Depends on**: Phase 9 (V1 complete — existing KB infrastructure)

**Research**: [[Cognitive Memory Systems Research]] — comparative analysis of MuninnDB, Augment Code, Letta/MemGPT, GitHub Copilot, Cognee, and git-based context patterns.

## Phase 18: Test Coverage & CI — ADR-026 Phases 2-3

**Goal**: Extend test coverage to the desktop (Tauri + React), establish CI pipeline with automated test runs, coverage gating, and lint enforcement. **See [[ADR-026: Automated Testing Strategy — Three-Phase Full-Stack Coverage]].**

**Progress**: Not started. Phase 1 (server tests) complete — epic `b11g` closed 2026-03-12, 456 tests passing, 47.26% line coverage baseline.

**Features** (to be broken down):
- Desktop Rust backend unit tests (Tauri commands, state management)
- React component tests (Vitest + Testing Library)
- E2E integration tests (Playwright or similar)
- CI pipeline: cargo test + clippy + coverage on PR
- Coverage gating thresholds
- Lint enforcement (rustfmt, eslint)

**Success criteria**:
1. Desktop Rust tests cover Tauri command handlers
2. React components have unit test coverage for key flows
3. CI blocks PRs that fail tests or introduce clippy warnings
4. Coverage trends tracked over time

**Depends on**: Phase 15 (deep module architecture — clean boundaries make testing easier)

## Phase Dependency Graph

```
Phase 1-9: V1 Complete
    |
    ├── Phase 10: Architecture & Agent Roles ✅
    │       |
    │   Phase 11: Own the Agent Loop ✅ (parallel with 10)
    │       |
    │   Phase 12: Backlog Grooming (nearly complete)
    │
    ├── Phase 13: Chat Experience (in progress)
    │
    ├── Phase 14: Desktop SSE Completeness (in progress)
    │
    ├── Phase 15: Deep Module Architecture (in progress)
    │       |
    │   Phase 18: Test Coverage & CI
    │
    ├── Phase 16: Operational Reliability
    │
    └── Phase 17: Cognitive Memory
```

Phases 13-15 can proceed in parallel. Phase 16 and 17 are independent of each other. Phase 18 benefits from Phase 15 clean boundaries.

## Coverage Check

Updated 2026-03-13. V1 phases 1-9 complete. Post-V1 phases 10-11 complete.

- Phase 1: DB-01..07, MCP-01/02/05, CFG-01/02 (13 reqs) ✅
- Phase 2: TASK-01..14 (14 reqs) ✅
- Phase 3: MEM-01..10 (10 reqs) ✅
- Phase 4: GIT-01..08, CFG-03 (9 reqs) ✅
- Phase 5: AGENT-01..11, AGENT-16..19, CFG-04 (16 reqs) ✅
- Phase 6: REVIEW-01..03 (3 reqs) ✅
- Phase 7: MCP-04, DB-05a, SYNC-01..04, WSL-01..04, LIFE-01..05 (16 reqs) ✅
- Phase 8: AGENT-19, OBS-01 extension (2 reqs) ✅
- Phase 9: GIT-09, OBS-02, CFG-02 MCP tools, REVIEW-01..03 completion (5 reqs) ✅
- Phase 10: ADR-014/015/024(PM)/slot architecture (cross-cutting) ✅
- Phase 11: AGENT-03/17/18/19 revised, OBS-03 (5 reqs) ✅
- Cross-cutting: TEST-01..03 (3 reqs) — Phase 1 complete (ADR-026, epic `b11g` closed 2026-03-12; 456 tests, all passing)

- Phase 16: AGENT-20..22, REVIEW-04 (4 reqs) — not started
- Phase 17: CMEM-01..14 (14 reqs) — not started

Total: 112+ (94 V1 + 5 Phase 11 + 4 Phase 16 + 14 Phase 17) ✓

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