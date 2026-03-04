---
tags:
    - planning
    - requirements
    - v1
title: V1 Requirements
type: requirement
---
# V1 Requirements — Djinn Server Rust Rewrite

Requirements derived from [[Project Brief]], [[Research Summary]], and the four research dimension notes. Each requirement traces to its source.

## Category: MCP (MCP Server Core)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| MCP-01 | Serve MCP tools over Streamable HTTP transport (rmcp 0.16+) | v1 | Brief: Core MCP Server |
| MCP-02 | Per-session server instances with shared state via Arc | v1 | Architecture Research §6 |
| MCP-03 | ~~MCP-connect bridge mode (stdio↔HTTP proxy) injecting project/task context into agent sessions~~ — DROPPED per ADR-008 (replaced by direct function calls via Goose extension) | ~~v1~~ | Brief: Core MCP Server |
| MCP-04 | SSE change feed: repository-emitted full-entity events streamed to desktop via SSE endpoint. Desktop updates UI directly from event payload — no follow-up reads. Covers creates, updates, deletes across all entity types. | v1 | ADR-002, Research Summary |
| MCP-05 | Tool registration organized by domain (task, memory, execution, system modules) | v1 | Architecture Research §6 |
| MCP-06 | `djinn-server --mcp-connect` stdio↔HTTP bridge mode for plugin compatibility. Forwards `tools/list` and `tools/call` over Streamable HTTP and discovers upstream URL from `~/.djinn/daemon.json` when not explicitly configured. | v1 | Compatibility requirement (desktop/plugin integrations) |

## Category: DB (Database Layer)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| DB-01 | Single rusqlite database at `~/.djinn/djinn.db` with WAL mode enabled | v1 | ADR-002, Brief |
| DB-02 | Connection discipline: single writer with `BEGIN IMMEDIATE`, `busy_timeout=5000`, `synchronous=NORMAL`, `foreign_keys=ON` | v1 | ADR-002, Research Summary |
| DB-03 | Periodic WAL checkpoint (`PRAGMA wal_checkpoint(PASSIVE)`) on ~30s background timer | v1 | ADR-002 |
| DB-04 | refinery 0.9 migrations with `int8-versions` for timestamp-based naming (V{YYYYMMDDHHMMSS}__desc.sql). Prevents AI ordering conflicts. Canonical schema.sql alongside migrations. | v1 | ADR-003 |
| DB-05 | Repository pattern: all writes through Repository structs with private Connection. Every write method emits full-entity event via broadcast channel. Compile-time enforcement (Rust visibility). | v1 | ADR-002, Research Summary |
| DB-05a | Desktop initial load / reconnect: read DB file directly (rusqlite read-only, WAL) for local mode; MCP tool reads for VPS fallback | v1 | ADR-002 |
| DB-06 | Canonical `schema.sql` committed alongside migration files | v1 | Pitfalls Research §4 |
| DB-07 | UUIDv7 (RFC 9562) for entity IDs (lexicographically sortable, stored as TEXT in canonical lowercase hex) | v1 | Architecture Research §5 |
| DB-08 | Vector search via sqlite-vec extension | v2 | Research Summary |

## Category: TASK (Task Board)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| TASK-01 | Epics as separate entities (own table, lifecycle: open → closed, no dependencies between epics) | v1 | Brief |
| TASK-02 | Tasks under epics only (flat hierarchy — no subtasks, no subepics) | v1 | Brief |
| TASK-03 | Task state machine: draft → open → in_progress → needs_task_review → in_task_review → approved → closed | v1 | Brief, Architecture Research §3 |
| TASK-04 | Typestate pattern at service layer for compile-time task transition correctness | v1 | Architecture Research §3, Research Summary |
| TASK-05 | Blocker dependencies between tasks (not epics) | v1 | Brief |
| TASK-06 | Short IDs (4-char, collision-resistant) alongside UUIDv7 | v1 | Brief |
| TASK-07 | Task CRUD via MCP tools (create, update, list, show, transition, delete) | v1 | Brief |
| TASK-08 | Activity log: append-only table with event_type, JSON payload, optional task_id (survives task deletion) | v1 | Architecture Research §5, Brief |
| TASK-09 | Board health and reconciliation (heal stale tasks, recover stuck sessions) | v1 | Brief |
| TASK-10 | Issue types: epic, feature, task, bug | v1 | Brief |
| TASK-11 | Labels, priority (0=highest), owner (git email) on tasks | v1 | Brief |
| TASK-12 | Comments on tasks (actor_id, actor_role, body, timestamp) | v1 | Brief |
| TASK-13 | Acceptance criteria on tasks (string[] or {criterion, met}[]) | v1 | Brief |
| TASK-14 | Design field on tasks (architecture notes, implementation guidance) | v1 | Brief |

## Category: MEM (Memory / Knowledge Base)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| MEM-01 | Notes with typed folders (adr, pattern, research, requirement, reference, design, brief, roadmap, etc.) | v1 | Brief |
| MEM-02 | FTS5 full-text search with BM25 ranking | v1 | Brief |
| MEM-03 | Wikilink graph: bidirectional links between notes, resolved at index time | v1 | Brief |
| MEM-04 | Memory↔task references: bidirectional lookup (memory_refs on tasks, task_refs from notes) | v1 | Brief |
| MEM-05 | Auto-generated catalog (table of contents for all notes) | v1 | Brief |
| MEM-06 | Note CRUD via MCP tools (write, read, edit, search, list, delete, move) | v1 | Brief |
| MEM-07 | Note git history tracking (diff, log per note file) | v1 | Brief |
| MEM-08 | Singleton types (brief, roadmap) — one per project, fixed file path | v1 | Brief |
| MEM-09 | Orphan detection (notes with zero inbound links) | v1 | Brief |
| MEM-10 | Broken link detection (wikilinks pointing to non-existent notes) | v1 | Brief |

## Category: AGENT (Agent Orchestration / Coordinator)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| AGENT-01 | Actor hierarchy (Ryhl hand-rolled pattern): 1× CoordinatorActor (global — dispatch decisions across all projects), 1× AgentSupervisor (global — tracks all running sessions up to capacity limit), N× GitActor (per-project — serializes git ops per repository). Event broadcasting handled by repository's broadcast::Sender, not a separate actor. Sessions are in-process tokio tasks via Goose library, not subprocesses (revised per ADR-008). | v1 | Architecture Research §1, ADR-008 |
| AGENT-02 | Three agent types: worker (developer), task reviewer, epic reviewer | v1 | Brief, Research Summary |
| AGENT-03 | Agent dispatch via Goose library — in-process async tasks, not subprocesses (revised per ADR-008, replaces summon crate) | v1 | Brief, ADR-008 |
| AGENT-04 | Model discovery from models.dev catalog + custom providers | v1 | Brief |
| AGENT-05 | Model health tracking: circuit breakers, cooldowns, auto-disable on repeated failures, rerouting to alternatives | v1 | Brief, Features Research |
| AGENT-06 | Session limiting (per-model capacity) | v1 | Brief |
| AGENT-07 | Event-driven dispatch (not polling) — `tokio::select!` with cancellation token + channel receivers | v1 | Brief, Stack Research |
| AGENT-08 | Stuck detection and recovery (30s interval tick as safety net) | v1 | Brief, Stack Research |
| AGENT-09 | Graceful shutdown: CancellationToken propagation to Goose agents (in-process), no SIGTERM/SIGKILL (revised per ADR-008) | v1 | Stack Research, Architecture Research §8, ADR-008 |
| AGENT-10 | WIP commits on graceful pause/shutdown (`--no-verify` — incomplete work) | v1 | Brief |
| AGENT-11 | Actor struct hard limits: ≤15 message variants per actor, ≤20 fields per struct | v1 | Architecture Research §1, Pitfalls Research §2 |
| AGENT-12 | ~~Scaffold system (deploy skills/prompts to projects for agent sessions)~~ — DROPPED per ADR-008 (replaced by Goose prompt API + embedded templates) | ~~v1~~ | Brief |
| AGENT-13 | Multi-model routing (premium for planning, cheap for execution) | v2 | Features Research |
| AGENT-14 | Attribution-based quality loop (track finding acceptance rates) | v2 | Features Research |
| AGENT-15 | Compute governance / ACU budgets per task | v2 | Features Research |
| AGENT-16 | Credential vault in Djinn DB with encrypted API key storage — supports VPS, WSL, standalone deployment | v1 | ADR-008 |
| AGENT-17 | Goose provider creation from vault credentials at dispatch time (runtime key management, no server restart) | v1 | ADR-008 |
| AGENT-18 | Per-session Goose Agent configuration: prompt override + extension scoping per agent type | v1 | ADR-008 |
| AGENT-19 | OAuth-capable Goose providers exposed through MCP: catalog advertises OAuth support and a tool can trigger provider OAuth setup without manual CLI configuration | v1 | ADR-008 |

## Category: REVIEW (Review System)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| REVIEW-01 | Task review: acceptance criteria verification + code nitpicks on individual task diffs | v1 | Brief |
| REVIEW-02 | Epic review: completeness check (missing tasks?) + aggregate code quality (patterns, duplicates, architectural drift). Reviewer receives a per-task list of `merge_commit_sha` values (see GIT-09) and inspects each via `git show <sha>` — no contiguous range, since tasks from other epics may be interleaved on the same branch. | v1 | Brief, GIT-09 |
| REVIEW-03 | Review rejection returns task to agent with feedback for rework | v1 | Brief (task state machine) |
| REVIEW-04 | Specialist review agents (correctness/security/performance/standards) | v2 | Features Research |

## Category: GIT (Git Integration)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| GIT-01 | Task branches created from target branch (configurable, default: main) on remote | v1 | Brief |
| GIT-02 | Agent works in isolated worktree (user's checkout untouched) | v1 | Brief, Features Research |
| GIT-03 | Squash-merge to target branch upstream on approval | v1 | Brief |
| GIT-04 | GitActor: serialize all git operations through a single actor per repository | v1 | Architecture Research §4 |
| GIT-05 | Hybrid git2 + CLI: git2 for reads (status, diff, ref queries), CLI for writes (worktree, merge, push) | v1 | Architecture Research §4 |
| GIT-06 | Worktree lifecycle: create, cleanup, orphan detection, `git worktree prune` before create | v1 | Brief, Pitfalls Research §6 |
| GIT-07 | Git hook awareness: capture pre-commit/commit-msg failures, re-dispatch agent to fix | v1 | Brief |
| GIT-08 | Target branch configurable per project | v1 | Brief |
| GIT-09 | Store `merge_commit_sha` on Task after squash-merge completes. Used by the epic reviewer to identify each task's individual contribution — a contiguous commit range is not usable because commits from other epics may be interleaved on the same target branch. Epic reviewer receives a per-task SHA list and runs `git show <sha>` per task. | v1 | REVIEW-02, ADR-008 |

## Category: SYNC (djinn/ Namespace Sync)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| SYNC-01 | SyncManager with pluggable channel registration. v1 channel: `djinn/tasks` (task state, JSONL per-user files). Future channels (not v1): `djinn/memory`, `djinn/settings`. | v1 | Brief, ADR-007 |
| SYNC-02 | Fetch-rebase-push per channel with conflict resolution (tasks channel: last-writer-wins on updated_at) | v1 | Brief, ADR-007 |
| SYNC-03 | Per-channel backoff schedule on push failures (30s → 15min exponential) | v1 | Brief, ADR-007 |
| SYNC-04 | Enable/disable per-machine (local DB flag) or team-wide (delete remote branch) | v1 | Brief, ADR-007 |
| SYNC-05 | Channel failure isolation — one channel failing does not block other channels | v1 | ADR-007 |

## Category: OBS (Observability)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| OBS-01 | Structured activity in DB for task lifecycle events (queryable from desktop) | v1 | Brief |
| OBS-02 | File-based operational log at `~/.djinn/` with levels and rotation (crashes, coordinator decisions) | v1 | Brief |
| OBS-03 | Step-level agent tracing | v2 | Features Research (89% of prod teams require it) |

## Category: LIFE (Server Lifecycle)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| LIFE-01 | Desktop spawns server as child process OR server runs standalone (VPS/WSL) — config passed via CLI args/env (revised per ADR-008 for standalone support) | v1 | ADR-005, ADR-008 |
| LIFE-02 | Graceful shutdown on SIGTERM: stop new connections, stop dispatch, CancellationToken → Goose agents stop → WIP-commit (30s timeout), WAL checkpoint, clean exit (revised per ADR-008 — CancellationToken, not SIGTERM/SIGKILL to agents) | v1 | ADR-005, ADR-008 |
| LIFE-03 | Graceful restart for updates: desktop signals SIGTERM → waits for exit → starts new binary → new server reads state from DB and resumes | v1 | ADR-005 |
| LIFE-04 | Board reconciliation on startup: detect interrupted agents (in_progress tasks with no running session), heal stale tasks, re-dispatch | v1 | ADR-005 |
| LIFE-05 | Desktop monitors server process (exit codes, health checks), restarts on unexpected crash | v1 | ADR-005 |

## Category: TEST (Testing)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| TEST-01 | Per-test DB isolation via Connection::open_in_memory() — each test gets a fresh DB with migrations applied | v1 | Research Summary |
| TEST-02 | Axum integration tests via tower::ServiceExt::oneshot() — test MCP tools end-to-end | v1 | Stack Research |
| TEST-03 | Time-dependent tests via tokio::test(start_paused = true) for stuck detection, timeouts, circuit breakers | v1 | Stack Research |

## Category: CFG (Configuration / Settings)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| CFG-01 | Settings stored in DB (replaces per-project JSON files) | v1 | Brief |
| CFG-02 | Project registry: add/remove/list projects | v1 | Brief |
| CFG-03 | Git settings per project (target branch, hook behavior) with global defaults | v1 | Brief |
| CFG-04 | Model configuration: capacity limits and routing preferences (narrowed per ADR-008 — credentials managed by vault + Goose providers) | v1 | Brief, ADR-008 |

## Category: WSL (WSL / Deployment)

| ID | Requirement | Classification | Source |
|---|---|---|---|
| WSL-01 | Bind server to `0.0.0.0` (works in both WSL NAT and mirrored modes) | v1 | Architecture Research §7 |
| WSL-02 | All data files on Linux filesystem, never on `/mnt/c/` | v1 | Architecture Research §7 |
| WSL-03 | HTTP over TCP for IPC (Unix domain sockets don't cross WSL boundary) | v1 | Architecture Research §7 |
| WSL-04 | Runtime detection of direct DB file access capability; fall back to MCP tool reads | v1 | ADR-002 |

## Out of Scope (Explicitly Excluded)

| Area | What | Why |
|---|---|---|
| Vector search / RAG | DiskANN, embeddings, semantic search | v2 — sqlite-vec supports it, but not needed for v1 |
| Multi-user / teams | Concurrent users, RBAC, shared workspaces | v2 — single-user-per-server for v1 |
| VPS deployment | Remote server with local desktop replica | v2 — architecture supports it but v1 targets local/WSL |
| Desktop open-source | Publishing the Electron app source | v2+ — not related to server |
| Hook bridge HTTP | Agent hook interception server | Deferred to summon v2 |
| Stacked branches | Phase-based stacked branch merging | Deliberately eliminated in rewrite |
| CDC pipeline | Change data capture triggers + polling | Eliminated by ADR-002 (repository events + SSE) |
| MCP LoggingMessage for data sync | Push notifications for data freshness | Eliminated by ADR-002 (SSE change feed with full entities) |
| Turso Cloud integration | Embedded replicas, Turso Sync | Eliminated by ADR-002 (rusqlite + WAL) |

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
| SYNC-* | Brief (scope section), ADR-007 |
| OBS-* | Brief, Features Research (89% need tracing) |
| CFG-* | Brief (scope section) |
| WSL-* | Architecture Research §7 (WSL considerations), ADR-002 |

## Relations

- [[Project Brief]] — primary source for v1 scope
- [[Research Summary]] — synthesis driving requirement priorities
- [[Database Layer — rusqlite over libsql/Turso]] — ADR-002 driving DB requirements
- [[Migrations — refinery with timestamp-based naming]] — ADR-003 driving migration requirements
- [[Server Lifecycle — Desktop-Managed Daemon with Graceful Restart]] — ADR-005 driving LIFE requirements
- [[Roadmap]] — phased delivery plan consuming these requirements
- [[Stack Research]] — crate versions and API patterns
- [[Features Research]] — market features informing v1/v2 classification
- [[Architecture Research]] — patterns informing design requirements
- [[Pitfalls Research]] — risks driving defensive requirements




## Phase 5 Task Traceability

| Task | Title | Requirements Covered |
|------|-------|---------------------|
| sy31 | Credential vault schema and repository | AGENT-16 |
| 1tal | Goose crate integration and agent module scaffold | AGENT-03 (partial) |
| lnfo | Embedded prompt templates for agent types | AGENT-02, AGENT-18 (partial) |
| ewkl | Djinn tools Goose extension | AGENT-03 (tool access) |
| rvmf | AgentSupervisor actor with session tracking and capacity | AGENT-06, AGENT-11 |
| xq5l | Agent dispatch flow with provider creation and per-session config | AGENT-03, AGENT-17, AGENT-18 |
| 951w | Graceful shutdown and WIP commits for agent sessions | AGENT-09, AGENT-10 |
| 1u1b | CoordinatorActor (closed) | AGENT-01, AGENT-07, AGENT-08, AGENT-11 |
| n8e4 | Model health (closed) | AGENT-04, AGENT-05, CFG-04 |
