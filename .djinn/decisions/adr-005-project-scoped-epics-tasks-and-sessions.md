---
title: ADR-005: Project-Scoped Epics, Tasks, and Sessions
type: adr
tags: ["adr","architecture","multi-project"]
---

# ADR-005: Project-Scoped Epics, Tasks, and Sessions

## Status: Accepted

## Context

The Djinn server supports multiple projects via the `projects` table (id, name, path). The `notes` table correctly references `project_id` as a FK. However, `epics`, `tasks`, and `sessions` have **no project_id column** — they exist in a single global namespace.

This is broken for multi-project use:
- The coordinator cannot determine which project directory to dispatch a task into
- The kanban/roadmap views cannot filter by project
- Epic short_ids collide across projects
- `task_list`, `epic_list`, etc. return items from all projects mixed together

Additionally:
- `tasks.epic_id` is `NOT NULL`, but tasks should be able to exist without an epic (standalone tasks)
- Task status CHECK still references `needs_phase_review`/`in_phase_review` — should be `needs_epic_review`/`in_epic_review` since phases were eliminated (ADR-009)

## Decision

### 1. Add `project_id` FK to epics, tasks, and sessions

New migration adds `project_id TEXT NOT NULL REFERENCES projects(id)` to:
- `epics` — direct FK to projects
- `tasks` — direct FK to projects (NOT inherited from epic, since tasks can be standalone)
- `sessions` — direct FK to projects

Indexes on `project_id` for all three tables.

### 2. Make `tasks.epic_id` nullable

Change from `epic_id TEXT NOT NULL REFERENCES epics(id)` to `epic_id TEXT REFERENCES epics(id) ON DELETE SET NULL` — allows standalone tasks without an epic.

### 3. Rename phase_review → epic_review in task status

Update the status CHECK constraint:
- `needs_phase_review` → `needs_epic_review`
- `in_phase_review` → `in_epic_review`

### 4. Update all MCP tools to require project context

All epic_*, task_*, and session_* MCP tools must accept and filter by project. The `project` parameter changes from "accepted for API compatibility, currently unused" to required (either project path or project ID).

Resolution: MCP tools accept project path (string), resolve to project_id via projects table.

### 5. Unique constraints scoped to project

- `epics.short_id` — unique per project (not globally)
- `tasks.short_id` — unique per project (not globally)

## Consequences

**Positive:**
- Coordinator knows which project directory to dispatch tasks into
- Desktop kanban/roadmap correctly filter by project
- Short IDs can be reused across projects (no global collision)
- Standalone tasks supported (backlog items, bugs without epic)

**Negative:**
- Breaking migration — existing data needs project_id backfilled
- All MCP tool handlers need project resolution logic
- All repository queries need project_id WHERE clause

## Relations
- [[ADR-003: Split Epic and Task MCP Tools with Input Validation]] — tool signatures affected
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]] — phase_review rename
- [[requirements/v1-requirements]] — KANBAN-08 project selector depends on this
