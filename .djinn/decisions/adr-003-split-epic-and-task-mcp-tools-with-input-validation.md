---
title: 'ADR-003: Split Epic and Task MCP Tools with Input Validation'
type: adr
tags: []
---

# ADR-003: Split Epic and Task MCP Tools with Input Validation

## Status: Accepted

## Context

The Djinn server stores epics and tasks in **separate database tables** with fundamentally different schemas:

- **Epics**: id, short_id, title, description, emoji, color, status (open/closed), owner, timestamps
- **Tasks**: id, short_id, epic_id, title, description, design, issue_type, status (10-state machine), priority, owner, labels, acceptance_criteria, reopen_count, continuation_count, memory_refs, merge_commit_sha, close_reason, blocked_from_status, timestamps

Despite being separate entities, epics have **zero dedicated MCP tools**. They are accessed through task tools via workarounds:
- `task_create(parent=epic_id)` — the `parent` param is actually an epic ID
- `task_children_list(epic_id)` — lists tasks under an epic
- `task_parent_get(id)` — returns the parent epic of a task
- `task_list(parent=epic_id)` — filters tasks by epic
- `task_list(issue_type="!epic")` — a no-op filter since epics are never in the tasks table

Additionally, the codebase has **no application-layer input validation** on any field. The only enforcement is SQLite CHECK constraints on `issue_type` and `status`. Fields like emoji, color, title, priority, labels, and owner are completely unvalidated.

## Decision

### 1. Split MCP tools into separate `epic_*` and `task_*` namespaces

**New epic tools (9):**
- `epic_create` — create an epic with epic-specific fields
- `epic_show` — show epic details enriched with task stats
- `epic_list` — list/filter/paginate epics
- `epic_update` — update epic-specific fields
- `epic_close` — transition open → closed
- `epic_reopen` — transition closed → open
- `epic_delete` — delete epic (CASCADE deletes child tasks)
- `epic_tasks` — list tasks under an epic with filters (replaces `task_children_list`)
- `epic_count` — count epics with optional grouping

**Removed task tools (2):**
- `task_parent_get` — replaced by reading `epic_id` from task + calling `epic_show`
- `task_children_list` — replaced by `epic_tasks`

**Modified task tools (6):**
- `task_create` — rename `parent` → `epic_id`
- `task_update` — rename `parent` → `epic_id`
- `task_list` — remove `parent` param (use `epic_tasks` instead)
- `task_count` — remove `parent` param, rename `group_by: "parent"` → `"epic"`
- `task_ready` — remove `issue_type` param (ready queue is always tasks)
- `task_claim` — remove `issue_type` param (claim always picks a task)

### 2. Add application-layer input validation on all fields

Validate at the MCP tool handler layer before reaching the repository/database. Return clear error messages with field name and constraint.

See [[Epic-Task MCP Split Design]] for full method signatures and validation rules.

## Consequences

**Positive:**
- Each MCP tool has a single clear responsibility
- LLM agents get better tool descriptions — no ambiguous "parent" params or "!epic" filters
- Validation catches bad input early with helpful error messages instead of cryptic SQLite errors
- Epic CRUD is first-class — desktop UI can call epic tools directly
- The `issue_type` field is no longer overloaded to exclude epics

**Negative:**
- Breaking change for all MCP consumers (desktop, CLI, agent coordinator)
- Net +7 MCP methods increases the tool surface area
- Dual update needed: server tools + desktop API client

## Relations
- [[Epic-Task MCP Split Design]] — full method signatures and validation rules
- [[brief]] — desktop communicates with server via MCP tools
- [[requirements/v1-requirements]] — KANBAN and ROAD views consume these tools
