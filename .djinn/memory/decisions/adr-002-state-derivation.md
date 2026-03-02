---
title: "ADR-002: State Derivation"
type: adr
tags:
  - architecture
  - state-derivation
  - phase-0
---
# ADR-002: State Derivation

## Status

Accepted

## Context

GSD tracks project state in a mutable STATE.md file (current phase, current
plan, accumulated context). Porting this pattern to Djinn memory would create
"ghost state" -- a stored snapshot that diverges from reality when tasks are
modified outside the workflow. Any stored state note becomes stale the moment
a task is moved, closed, or created by another agent or manual action.

## Decision

All progress is **derived from live task board queries**. No stored state
notes. No STATE.md equivalent in Djinn memory.

Routing uses a **roadmap + board hybrid**:

1. Roadmap memory note provides intent (which milestone, what success criteria)
2. Board queries provide current status

The roadmap memory note is **immutable** -- it captures original intent and
success criteria. Scope changes produce a new version, not edits. Workflows
MUST NEVER call `memory_edit` on the roadmap note.

### Progress Query Chain (pseudocode)

```
# "Where are we?" -- derive from live board
roadmap = memory_read(identifier="roadmap")
all_tasks = task_count(project=P, group_by="status")
ready = task_ready(project=P, issue_type="!epic")
in_progress = task_list(project=P, status="in_progress")
phases = execution_phase_list(project=P)

# "What's next?" -- route based on state
if in_progress.total > 0:  route -> "monitor active work"
if ready.total > 0:        route -> "launch execution"
if all_closed(milestone):  route -> "milestone complete"
else:                      route -> "plan next milestone"
```

## Consequences

- **Good**: State is always consistent -- no stale snapshots
- **Good**: Multiple agents can query simultaneously without conflicts
- **Good**: No state synchronization bugs between workflows
- **Bad**: Every progress check requires multiple MCP calls
- **Mitigation**: Query chain is fast (< 1 second total) and cacheable
  within a single agent session

## Relations

- [[ADR-001: Hierarchy Mapping]]
- [[Artifact Mapping]]
- [[Roadmap]]
