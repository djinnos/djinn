---
title: ADR-015: Session Continuity & Resume
type: adr
tags: ["adr","execution","session","resume"]
---


# ADR-015: Session Continuity & Resume

## Status
Accepted

## Date
2026-03-04

## Context

Currently, every session is a one-shot: worker finishes → session ends → worktree cleaned up. When a task needs rework (reviewer rejection, merge conflict, verification failure), a **fresh session** is dispatched. The new agent must rediscover the codebase, re-read files, and rebuild context from scratch. This wastes tokens and time — the original agent already had full context.

Three scenarios trigger rework today:
1. **Verification failure** (new with [[ADR-014: Project Setup & Verification Commands]]): post-session build/test fails
2. **Task reviewer rejection**: reviewer sends feedback, task reopens, new worker session dispatched
3. **Merge conflict**: conflict detected after merge attempt, ConflictResolver session dispatched

In all three cases, the original worker agent had the context needed to resolve the issue efficiently. Spawning a fresh session discards that context.

## Decision

Sessions become **long-lived and resumable**. A session only truly completes when the task is approved/closed. Between interactions, the session is **persisted and the capacity slot is freed**.

### Lifecycle

```
Worker session starts
  → agent does work → signals DONE
  → session freed (capacity slot released, GooseAgent dropped)
  → verification runs as code
  → if fails: resume session with failure output → agent fixes → re-verify → loop
  → if passes: submit for review
  → reviewer runs
  → if rejects: resume original worker session with feedback → agent fixes → re-verify → resubmit
  → if conflict: resume original worker session with conflict info → agent resolves
  → session truly completes when task approved/closed
```

### Resume Mechanism

- When a session needs to pause: record the Goose session ID, end the GooseAgent, free the capacity slot
- When resuming: create a new GooseAgent, load conversation history from Goose's session storage (SQLite at `~/.djinn/sessions/`), continue the conversation
- The worktree stays alive between resume cycles (not cleaned up until task closes)
- Djinn's session record tracks the logical session across multiple GooseAgent lifetimes

### Capacity Management

- Paused sessions do NOT hold capacity slots — only active GooseAgent instances count
- A task with a paused session waiting for review consumes zero model capacity
- On resume, capacity is re-checked — if the model is at capacity, the resume queues like any other dispatch

### Retry Behavior

- Unlimited verification retries — the agent can self-block via `WORKER_RESULT: BLOCKED` if it determines it cannot fix the issue
- Agent progress is implicit — if the agent is making tool calls and changing code, it's making progress

## Consequences

**Positive:**
- Eliminates redundant context discovery — agent resumes with full conversation history
- Significant token savings on rework cycles (no re-reading files, no re-understanding codebase)
- Faster rework — agent immediately knows what it was doing and what went wrong
- Cleaner conflict resolution — same agent that wrote the code resolves its own conflicts
- Capacity-efficient — paused sessions don't block other tasks

**Negative:**
- Worktrees persist longer (until task closes) — more disk usage
- Goose session storage grows with long conversations
- Resume depends on Goose's session persistence reliability
- Conversation history may grow large for tasks with many rework cycles (context window pressure)

## Relations

- [[Roadmap]] — Post-V1 enhancement
- [[ADR-014: Project Setup & Verification Commands]] — Verification failures are the primary trigger for session resume
- [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning|ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — Goose session storage enables resume
- [[ADR-010: Session Cost Tracking — Per-Task Token Metrics]] — Token tracking spans the full logical session lifecycle
