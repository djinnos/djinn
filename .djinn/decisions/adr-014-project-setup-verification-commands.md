---
title: ADR-014: Project Setup & Verification Commands
type: adr
tags: ["adr","execution","setup","verification"]
---


# ADR-014: Project Setup & Verification Commands

## Status
Accepted

## Date
2026-03-04

## Context

Each codebase has project-specific quirks that agents must discover ad-hoc: dependency installation, build prerequisites, binary placement, linting tools. This wastes tokens, introduces non-determinism, and causes failures when agents miss required steps.

Task `nyf9` on the desktop project hit this directly — the conflict resolver completed its work but blocked because `src-tauri/binaries/djinn-server-x86_64-unknown-linux-gnu` was missing from a fresh worktree. This is a build prerequisite that no agent should have to figure out.

Stripe's Minions system (March 2026) validates this approach: their "blueprints" intermix deterministic code nodes (linting, CI, dependency install) with agentic decision nodes. Deterministic nodes run without LLM involvement — cheaper, faster, more reliable.

Currently Djinn has zero support for pre/post-session automation. The worker prompt says "run the project's build and test commands" but the agent discovers them every time. The task reviewer does acceptance criteria checking only with no automated build/test validation.

## Decision

Add per-project **setup commands** and **verification commands** stored in the project registry (DB). These run as deterministic code — no LLM involvement.

### Command Types

- **Setup commands**: Run in the task worktree *before* the agent starts. Install dependencies, place binaries, generate code. If these fail, the task cannot proceed.
- **Verification commands**: Run in the task worktree *after* the agent finishes work. Build checks, test suites, linting. If these fail, the output is fed back to the agent for fixing.

### Validation at Configuration Time

When a user adds or updates commands via MCP tools, Djinn:
1. Creates a temporary worktree from the project's target branch
2. Runs setup commands in order
3. Runs verification commands in order
4. If all pass (exit 0), saves the configuration
5. If any fail, shows the output to the user and does NOT save

This ensures commands are known-good at config time. No ambiguity about whether the command itself is broken.

### Validation on Execution Start

When `execution_start` is called, for each project with configured commands:
1. Create a temporary worktree
2. Run setup → verification
3. If any fail, mark the project as "setup unhealthy" — do not dispatch tasks for that project
4. Healthy projects dispatch normally — per-project health, not global blocking
5. Surface failures to the user via SSE events

### Per-Task Execution

- **Pre-dispatch**: Setup commands run in the task worktree after git preparation, before the agent session starts
- **Post-session**: Verification commands run in the task worktree after the agent signals completion, before status transition
- **Failure handling**: Verification failures are fed back to the same session (see [[ADR-015: Session Continuity & Resume]]). The agent fixes the issue, then verification re-runs. Unlimited retries — the agent can self-block via `WORKER_RESULT: BLOCKED` if it can't resolve the issue.

### Agent Prompt Integration

- **Worker prompt**: Told what commands run automatically — "do not run these yourself." Lists setup and verification commands explicitly.
- **Task reviewer prompt**: Told verification already passed — "focus on acceptance criteria and code quality, do not re-run builds or tests."

### Storage & MCP Tools

Commands stored per-project in the project registry (DB). New MCP tools for management (exact tool names TBD during implementation):
- View configured commands for a project
- Add/update/remove setup and verification commands (triggers worktree validation)

## Consequences

**Positive:**
- Eliminates token waste from agents discovering build/test commands
- Deterministic verification — same commands every time, no LLM variance
- Catches environment issues at config time and on startup, not mid-session
- Task reviewers skip redundant build/test runs
- Per-project health prevents dispatching to broken environments

**Negative:**
- Worktree creation overhead at config time and on execution start (acceptable — one-time validation)
- Per-task setup adds wall-clock time before agent starts (same cost as agent doing it, just shifted)
- Commands may drift if project structure changes between validation runs (mitigated by execution_start re-validation)

## Relations

- [[Roadmap]] — Post-V1 enhancement
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — Extends execution model with pre/post hooks
- [[ADR-015: Session Continuity & Resume]] — Verification failures trigger session resume
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — Goose sessions are the unit being wrapped
