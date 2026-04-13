---
title: ADR-007: Session Viewer — Unified Chat UI for Task Execution
type: adr
tags: ["adr","session-viewer","ui"]
---


## Context

When a task executes, multiple agents participate (worker, task_reviewer, conflict_resolver) across multiple sessions, with compaction boundaries, setup/verification command runs, and system events (submitted for review, reopened, merged). Users need to see what happened during execution — not just the final status, but the full story.

Reference implementation studied: Craft Agents OSS renders sessions with TurnCard components, activity grouping, and nested subagent trees. Our model is simpler (no real-time streaming to desktop yet) but needs to convey the same information.

## Decision

Render task execution as a **single unified chat thread** — not segmented by session, round, or phase. The chat contains three visual block types:

### 1. Agent Bubbles (the conversation)
- Each agent type gets a distinct visual identity (icon + border color): 🔧 Worker, 🔍 Reviewer, ⚔ Conflict Resolver
- Messages flow naturally: agent text, tool calls (collapsed one-liners, click to expand), subagent runs (same treatment)
- Continuation sessions after compaction are seamless — subtle `(cont.)` label, no break in the chat
- Compaction boundaries appear as inline dividers: `─ ⊘ compacted (80% · 322k/400k) ─`

### 2. Command Blocks (setup/verification)
- Rendered inline in the chat flow at the point they actually ran
- Each command is a collapsible row: `▸ command name ··· ✓/✗  duration`
- Successful commands: collapsed by default, just name + checkmark + duration
- Failed commands: auto-expanded showing stdout/stderr output
- Setup runs before the agent session; verification runs after worker signals DONE
- Verification failure → agent fix → re-verification reads naturally top to bottom

### 3. System Dividers
- Status transitions rendered as centered dividers (like date separators in messaging apps)
- Examples: `── submitted for review ──`, `── reopened ──`, `── merged to main ──`
- Lightweight, not clickable, just structural markers

### Layout
- Full-page view at `/task/:id` route for tasks with sessions
- Left panel: task details (description, criteria, metadata) — similar to current TaskDetailPanel
- Right panel: the unified chat thread, scrollable
- Tasks without sessions: current slide-over modal (TaskDetailPanel)

### Data Sources
- Agent chat content: new Goose session messages API (see [[roadmap]])
- Setup/verification results: new structured activity events (see [[ADR-020: Structured Activity Events for Command Runs]])
- Status transitions, reviewer comments, compaction events: existing `task_activity_list` MCP tool
- Session metadata and continuation chains: existing `session_list(chain_ordered=true)` MCP tool

### Assembly Strategy
The UI fetches all data sources, then interleaves them chronologically into a single ordered stream:
1. `session_list(task_id, chain_ordered=true)` → session metadata + ordering
2. For each session: fetch Goose messages (new API)
3. `task_activity_list(id)` → status changes, commands, comments, compaction
4. Merge all events by timestamp into one timeline

## Consequences

**Positive:**
- Intuitive — reads like a conversation, not a dashboard
- Full context — user sees exactly why a task was rejected, how the agent fixed verification failures, what commands ran
- Matches mental model — agents are participants in a group chat working on the task

**Negative:**
- Requires new server API for Goose session messages (not currently exposed)
- Requires new structured activity events for command runs (currently only failures logged as comments)
- Long-running tasks with many rounds will produce long threads — may need virtualized scrolling
- No real-time streaming yet (SSE doesn't push session events) — initial version will be read-after-completion

## Relations
- [[roadmap]]
- [[ADR-006: Desktop Uses MCP SDK Directly from Frontend]]
- [[requirements/v1-requirements]]