---
tags:
    - planning
    - brief
title: Project Brief
type: brief
---
# Djinn Server — Rust Rewrite

## Vision

A Rust-native server that replaces the existing Go Djinn server with a fundamentally simpler architecture. The server is the brain of Djinn: it manages the task board, knowledge base, agent orchestration, and git integration via MCP tools. The rewrite eliminates the overcomplicated phase/stacked-branch system, merges separate databases into a single libSQL instance, and uses Turso embedded replicas to give the desktop zero-latency state access without CDC plumbing.

## Problem

The current Go server has accumulated significant complexity:

- **Phase system is overcomplicated.** Stacked branches, rebase state machines, three conflict types (intra-phase, cross-phase, auto-merge), per-phase merge mutexes. The coordinator is a god object with 50+ fields and 32 migrations of schema churn.
- **Epics are modeled as tasks.** Same table, same schema, filtered by `issue_type`. This leaks task lifecycle concerns (statuses, dependencies, phase assignment) into what should be a simple grouping concept.
- **Two separate databases per project.** Tasks in `tasks.db`, memory in `memory.db`, each with their own CDC change tables — doubled maintenance, doubled sync complexity.
- **CDC pipeline is brittle.** SQLite triggers → change tail goroutine → SSE → MCP re-fetch. Complex, keeps going out of sync, 150ms polling floor for change detection.
- **Go's compiler misses critical bugs.** AI-generated Go code has 2x more concurrency bugs than human code, and the compiler catches none of them (see [[Language Selection — Compiler as AI Code Reviewer]]).

## Target Users

- **Primary:** Djinn desktop app (Electron) — consumes MCP tools, receives events, reads DB replica
- **Secondary:** AI agents (Claude Code, OpenCode, etc.) — interact with task board and memory via MCP tools during autonomous sessions
- **Tertiary:** Fernando (developer/operator) — configures, monitors, debugs the system

## Success Metrics

- Server starts and serves MCP tools without external auth dependencies
- Desktop reads task/memory state from a local Turso embedded replica with no CDC pipeline
- Agents can be dispatched to tasks, work in worktrees, and merge directly to main upstream
- Epic review gates epic completion by checking for missing tasks and reviewing aggregate code quality
- Task review checks acceptance criteria and code nitpicks on individual task diffs
- All state (tasks, memory, projects, settings, activity) lives in a single libSQL database at `~/.djinn/`
- Works across deployment modes: local, WSL (server in WSL + desktop on Windows), VPS (server remote + desktop local)

## Constraints

- **Language:** Rust (decided — [[Language Selection — Compiler as AI Code Reviewer]])
- **Database:** libSQL/Turso (decided — [[Embedded Database Survey]])
- **Stack:** Axum + Tokio + Serde + Clap (decided — [[Rust Agentic Ecosystem Survey]])
- **Agent harness:** Goose library (in-process async tasks) — see [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]]
- **Git integration:** Task branches merge directly to main upstream. No stacked branches. Local repo untouched.
- **Hierarchy:** Epics are separate entities (not tasks). Tasks live under epics. No subepics, no subtasks. Flat.
- **Observability:** Structured activity in DB for task lifecycle. File-based operational log at `~/.djinn/` for system events/crashes.

## Scope
### In (v1)

**Core MCP Server:**
- MCP server (Streamable HTTP) with task, memory, execution, and system tools
- ~~MCP-connect bridge mode~~ — DROPPED per ADR-008 (replaced by direct function calls via Goose extension)

**Database:**
- Single libSQL database at `~/.djinn/` — tasks, epics, memory notes, projects, settings, activity, model health
- Turso embedded replica for desktop (local file or network sync)
- Settings stored in DB (replaces JSON files): projects, model config, git settings, sync config

**Task Board:**
- Epics as separate entities (own table, simplified lifecycle: open → closed, no dependencies)
- Tasks under epics only (flat hierarchy, no subtasks, no subepics)
- Task state machine: draft → open → in_progress → needs_task_review → in_task_review → approved → closed
- Blocker dependencies between tasks (not epics)
- Short IDs (4-char, collision-resistant)
- Activity log (structured, in DB — task lifecycle, comments, agent metadata)
- Board health and reconciliation

**Memory / Knowledge Base:**
- Notes with types (adr, pattern, research, requirement, reference, design, brief, roadmap, etc.)
- FTS5 full-text search with BM25 ranking
- Wikilink graph (bidirectional links between notes)
- Memory↔task references (bidirectional lookup)
- Catalog auto-generation

**Agent Orchestration (Coordinator):**
- Three agent types: worker (developer), task reviewer, epic reviewer
- Agent dispatch via Goose library (in-process async tasks, not subprocesses — ADR-008)
- Model discovery (models.dev catalog + custom providers) — server picks model, Goose runs agent in-process
- Model health tracking (circuit breakers, cooldowns, auto-disable, rerouting)
- Session limiting (per-model capacity)
- Event-driven dispatch (not polling)
- Stuck detection and recovery
- ~~Scaffold system~~ — DROPPED per ADR-008 (replaced by embedded prompt templates via include_str!())
- Credential vault in Djinn DB for API keys (supports VPS/WSL/standalone — ADR-008)

**Review System:**
- Task review: acceptance criteria verification + code nitpicks on individual diffs
- Epic review: completeness check (missing tasks?) + aggregate code quality (patterns, duplicates, architectural drift)

**Git Integration:**
- Task branches created from target branch (configurable, default: main) on remote
- Agent works in isolated worktree (user's checkout untouched)
- Squash-merge to target branch upstream on approval
- WIP commits on graceful pause/shutdown (always `--no-verify` — incomplete work, hooks are irrelevant)
- Git hook awareness: if pre-commit/commit-msg hooks reject an agent commit or coordinator merge, capture the error and re-dispatch the agent to fix (lint, format, etc.) before retrying
- Worktree lifecycle (create, cleanup, orphan detection)
- Target branch configurable per project

**Task Sync:**
- Sync state via `djinn/tasks` git branch (JSONL format, per-user files)
- Fetch-rebase-push with conflict resolution (last-writer-wins on updated_at)
- Backoff schedule on failures
- Enable/disable per-machine or team-wide

**Observability:**
- Structured activity in DB for task lifecycle events (queryable from desktop)
- File-based operational log at `~/.djinn/` with levels and rotation (crashes, coordinator decisions, system events)

**Desktop Events:**
- MCP LoggingMessage notifications for task execution, coordinator lifecycle, model health
- Turso replica handles state reads — events are just change signals

### Out (v2+)
- Vector search / RAG (DiskANN — libSQL supports it, but not v1)
- Multi-user / team collaboration
- VPS deployment mode (architecture supports it via Turso network sync, but v1 targets local/WSL)
- Open-sourcing the desktop
- Hook bridge HTTP server for agent hook interception (deferred)
## Relations
- [[Language Selection — Compiler as AI Code Reviewer]] — ADR driving language choice
- [[Embedded Database Survey]] — ADR driving database choice
- [[Rust Agentic Ecosystem Survey]] — stack ecosystem research
- [[V1 Requirements]] — detailed requirement breakdown
- [[Roadmap]] — phased delivery plan
- [[Stack Research]] — Rust server stack deep dive
- [[Features Research]] — feature analysis for task orchestration systems
- [[Architecture Research]] — system architecture patterns
- [[Pitfalls Research]] — risks and anti-patterns to avoid
