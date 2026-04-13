---
title: Setup & Verification Scope
type: reference
tags: ["scope","reference","setup","verification"]
---


# Setup & Verification Scope

## In Scope

- Per-project setup commands stored in project registry (DB) — ordered list of shell commands that run pre-agent in task worktrees
- Per-project verification commands stored in project registry (DB) — ordered list of shell commands that run post-agent in task worktrees
- Worktree-based validation when configuring commands — commands must pass in a fresh worktree to be saved
- Validation on `execution_start` — per-project health gate, unhealthy projects skipped (not global block)
- Setup execution in task worktree before agent session starts (deterministic, no LLM)
- Verification execution in task worktree after agent signals DONE (deterministic, no LLM)
- Verification failure → resume same Goose session with failure output → agent fixes → re-verify loop
- Reviewer rejection → resume original worker session with feedback (instead of fresh session)
- Merge conflict → resume original worker session with conflict info (instead of ConflictResolver)
- Worker prompt injection: list setup/verification commands, tell agent not to run them
- Task reviewer prompt injection: tell reviewer verification already passed, skip build/test
- New MCP tools: view/add/update/remove setup and verification commands per project
- Session resume via Goose session ID — free capacity slot when paused, re-acquire on resume
- Worktrees persist until task closes (not cleaned up between session resume cycles)
- Per-project health surfaced via SSE events to desktop
- Unlimited verification retries — agent self-blocks if it can't resolve

## Out of Scope

- Auto-detection of project type and command suggestions — deferred, not needed for initial implementation
- Caching/symlinking of build artifacts between worktrees (node_modules, target/) — tools have good built-in caching
- Per-task setup commands (only per-project) — keep it simple
- Lockfile hash-based validation skipping — always re-validate on execution_start for now
- Desktop UI for configuring commands — MCP tools first, desktop can consume later

## Preferences

- Commands stored in same DB as project registry, not in `.djinn/` config files
- Validation always uses fresh worktree to catch real-world agent scenarios
- Per-project health, not global — don't punish healthy projects
- Session resume over fresh session — preserve agent context across rework cycles
- Agent self-regulation for retry limits — no artificial cap

## Relations

- [[Roadmap]] — Post-V1 enhancement
- [[ADR-014: Project Setup & Verification Commands]] — Primary design decision
- [[ADR-015: Session Continuity & Resume]] — Session lifecycle redesign enabling verification feedback loop
- [[decisions/adr-009-simplified-execution-—-no-phases,-direct-task-dispatch|ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — Execution model being extended
- [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning|ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — Goose session storage enables resume
- [[V1 Requirements]] — Extends AGENT-03 (dispatch flow) and REVIEW-01 (task review)
