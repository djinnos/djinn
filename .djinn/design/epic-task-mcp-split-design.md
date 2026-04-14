---
tags:
    - design
    - mcp
    - server
    - api
    - validation
title: Epic-Task MCP Split Design
type: design
---
# Epic-Task MCP Split Design

Full method signatures, validation rules, and return types for the epic/task MCP tool split defined in [[decisions/adr-003-split-epic-and-task-mcp-tools-with-input-validation|ADR-003: Split Epic and Task MCP Tools with Input Validation]].

---

## Validation Rules Reference

All validation is enforced at the MCP tool handler layer before reaching the repository. Errors return `{ "error": "field_name: message" }`.

### Shared Field Constraints

| Field | Type | Constraints |
|-------|------|-------------|
| `title` | String | Required, 1–200 chars, trimmed, no leading/trailing whitespace |
| `description` | String | Optional, max 10,000 chars |
| `owner` | String | Optional, max 100 chars, trimmed |
| `emoji` | String | Optional, must be a single Unicode emoji (grapheme cluster from Emoji category), max 1 grapheme. Empty string allowed (clears field) |
| `color` | String | Optional, must be a valid 3, 4, 6, or 8-digit hex color (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`), case-insensitive. Empty string allowed (clears field) |
| `priority` | i64 | Optional, range 0–99 (0 = highest priority) |
| `issue_type` | String | Must be one of: `"task"`, `"feature"`, `"bug"`. Default: `"task"` |
| `labels` | Vec | Each label: 1–50 chars, trimmed, no empty strings. Max 20 labels per task |
| `design` | String | Optional, max 50,000 chars |
| `acceptance_criteria` | Vec | Max 50 items per task |
| `sort` | String | Must be one of the allowed sort values per tool |
| `limit` | i64 | Range 1–200, default 50 |
| `offset` | i64 | Range 0+, default 0 |

### ID Resolution

All `id` / `epic_id` params accept either a full UUID or a 4-char short_id. Resolution queries the appropriate table (epics or tasks). If not found, return `{ "error": "epic not found: {id}" }` or `{ "error": "task not found: {id}" }`.

---

## Epic Tools

### `epic_create`

Create a new epic.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `title` | String | yes | 1–200 chars, trimmed |
| `description` | String | no | max 10,000 chars |
| `emoji` | String | no | single emoji grapheme or empty string |
| `color` | String | no | hex color (`#rrggbb` etc.) or empty string |
| `owner` | String | no | max 100 chars, trimmed |

**Returns:** Epic object

```json
{
  "id": "uuid",
  "short_id": "a1b2",
  "title": "...",
  "description": "...",
  "emoji": "🚀",
  "color": "#8b5cf6",
  "status": "open",
  "owner": "...",
  "created_at": "ISO-8601",
  "updated_at": "ISO-8601",
  "closed_at": null
}
```

---

### `epic_show`

Show an epic with child task statistics.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | UUID or short_id, must resolve to existing epic |

**Returns:** Epic object enriched with stats

```json
{
  "id": "uuid",
  "short_id": "a1b2",
  "title": "...",
  "description": "...",
  "emoji": "🚀",
  "color": "#8b5cf6",
  "status": "open",
  "owner": "...",
  "created_at": "ISO-8601",
  "updated_at": "ISO-8601",
  "closed_at": null,
  "task_count": 12,
  "open_count": 4,
  "in_progress_count": 2,
  "closed_count": 6
}
```

---

### `epic_list`

List epics with filtering and pagination.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `status` | String | no | `"open"` or `"closed"` |
| `text` | String | no | full-text search on title + description |
| `sort` | String | no | `"created"`, `"created_desc"`, `"updated"`, `"updated_desc"`. Default: `"created"` |
| `limit` | i64 | no | 1–200, default 50 |
| `offset` | i64 | no | 0+, default 0 |

**Returns:**

```json
{
  "epics": [...],
  "total_count": 15,
  "limit": 50,
  "offset": 0,
  "has_more": false
}
```

---

### `epic_update`

Update an existing epic.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing epic |
| `title` | String | no | 1–200 chars if provided |
| `description` | String | no | max 10,000 chars |
| `emoji` | String | no | single emoji grapheme or empty string |
| `color` | String | no | hex color or empty string |
| `owner` | String | no | max 100 chars, trimmed |

**Returns:** Updated epic object

---

### `epic_close`

Close an open epic.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing epic with status `"open"` |

**Returns:** Updated epic object (status = `"closed"`, `closed_at` set)

**Error:** `"epic is already closed"` if status is `"closed"`

---

### `epic_reopen`

Reopen a closed epic.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing epic with status `"closed"` |

**Returns:** Updated epic object (status = `"open"`, `closed_at` cleared)

**Error:** `"epic is already open"` if status is `"open"`

---

### `epic_delete`

Delete an epic and all its child tasks (CASCADE).

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing epic |

**Returns:** `{ "ok": true, "deleted_task_count": N }`

---

### `epic_tasks`

List tasks under a specific epic. Replaces `task_children_list`.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `epic_id` | String | yes | must resolve to existing epic |
| `status` | String | no | valid task status |
| `issue_type` | String | no | `"task"`, `"feature"`, or `"bug"` |
| `sort` | String | no | `"priority"` (default), `"created"`, `"created_desc"`, `"updated"`, `"updated_desc"`, `"closed"` |
| `limit` | i64 | no | 1–200, default 50 |
| `offset` | i64 | no | 0+, default 0 |

**Returns:**

```json
{
  "tasks": [...],
  "total_count": 8,
  "limit": 50,
  "offset": 0,
  "has_more": false
}
```

---

### `epic_count`

Count epics with optional grouping.

**Params:**

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `status` | String | no | `"open"` or `"closed"` |
| `group_by` | String | no | `"status"` |

**Returns:** `{ "total_count": N }` or `{ "groups": [{ "key": "open", "count": 5 }, ...] }`

---

## Modified Task Tools

### `task_create` (CHANGED)

**Param rename:** `parent` → `epic_id`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `epic_id` | String | yes | must resolve to existing epic |
| `title` | String | yes | 1–200 chars, trimmed |
| `issue_type` | String | no | `"task"`, `"feature"`, `"bug"`. Default: `"task"` |
| `description` | String | no | max 10,000 chars |
| `design` | String | no | max 50,000 chars |
| `priority` | i64 | no | 0–99, default 0 |
| `owner` | String | no | max 100 chars, trimmed |
| `labels` | Vec | no | each 1–50 chars, max 20 labels |
| `acceptance_criteria` | Vec | no | max 50 items |

**Returns:** Task object

---

### `task_update` (CHANGED)

**Param rename:** `parent` → `epic_id`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |
| `title` | String | no | 1–200 chars if provided |
| `description` | String | no | max 10,000 chars |
| `design` | String | no | max 50,000 chars |
| `priority` | i64 | no | 0–99 |
| `owner` | String | no | max 100 chars, trimmed |
| `labels_add` | Vec | no | each 1–50 chars |
| `labels_remove` | Vec | no | each must be non-empty |
| `acceptance_criteria` | Vec | no | max 50 items (full replacement) |
| `epic_id` | String | no | must resolve to existing epic (re-parents the task) |
| `memory_refs_add` | Vec | no | each must be non-empty permalink |
| `memory_refs_remove` | Vec | no | each must be non-empty |

**Additional validation:** After merging `labels_add`, total labels must not exceed 20.

**Returns:** Updated task object

---

### `task_list` (CHANGED)

**Removed params:** `parent` (use `epic_tasks` instead)

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `status` | String | no | valid task status |
| `issue_type` | String | no | `"task"`, `"feature"`, `"bug"` |
| `priority` | i64 | no | 0–99 |
| `label` | String | no | 1–50 chars |
| `text` | String | no | full-text search |
| `sort` | String | no | `"priority"` (default), `"created"`, `"created_desc"`, `"updated"`, `"updated_desc"`, `"closed"` |
| `limit` | i64 | no | 1–200, default 50 |
| `offset` | i64 | no | 0+, default 0 |

**Note:** The `"!epic"` negation pattern is removed from `issue_type`. Only positive values are accepted.

**Returns:** `{ "tasks": [...], "total_count", "limit", "offset", "has_more" }`

---

### `task_count` (CHANGED)

**Removed params:** `parent`
**Changed:** `group_by: "parent"` → `group_by: "epic"`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `status` | String | no | valid task status |
| `issue_type` | String | no | `"task"`, `"feature"`, `"bug"` |
| `priority` | i64 | no | 0–99 |
| `label` | String | no | 1–50 chars |
| `text` | String | no | full-text search |
| `group_by` | String | no | `"status"`, `"priority"`, `"issue_type"`, `"epic"` |

**Returns:** `{ "total_count" }` or `{ "groups": [...] }`

---

### `task_ready` (CHANGED)

**Removed params:** `issue_type` (ready queue is always tasks, never epics)

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `label` | String | no | 1–50 chars |
| `owner` | String | no | max 100 chars |
| `priority_max` | i64 | no | 0–99 |
| `limit` | i64 | no | 1–200, default 50 |

**Returns:** `{ "tasks": [...] }`

---

### `task_claim` (CHANGED)

**Removed params:** `issue_type`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `label` | String | no | 1–50 chars |
| `owner` | String | no | max 100 chars |
| `priority_max` | i64 | no | 0–99 |
| `session_id` | String | no | valid UUID format |

**Returns:** Task object or `{ "task": null }` if none available

---

## Unchanged Task Tools

These tools require no signature changes but gain input validation:

### `task_show`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |

### `task_transition`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |
| `action` | String | yes | valid `TransitionAction` (already validated via enum parse) |
| `reason` | String | conditional | required for reject/reopen/release/block/force_close actions, max 2,000 chars |
| `target_status` | String | conditional | required when `action = "user_override"`, valid `TaskStatus` |
| `actor_id` | String | no | max 100 chars |
| `actor_role` | String | no | max 50 chars |

### `task_comment_add`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |
| `body` | String | yes | 1–10,000 chars, trimmed |
| `actor_id` | String | no | max 100 chars |
| `actor_role` | String | no | max 50 chars |

### `task_blockers_add`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |
| `blocking_id` | String | yes | must resolve to existing task, must not equal `id` |

**Note:** The `resolve_task_not_epic` guard is simplified — just resolve in the tasks table. If not found, return `"task not found"`.

### `task_blockers_remove`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |
| `blocking_id` | String | yes | must resolve to existing task |

### `task_blockers_list`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |

### `task_blocked_list`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |

### `task_activity_list`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | no | if provided, must resolve to existing task |
| `event_type` | String | no | if provided, must be known event type |
| `from_time` | String | no | valid ISO-8601 datetime |
| `to_time` | String | no | valid ISO-8601 datetime, must be >= `from_time` if both provided |
| `limit` | i64 | no | 1–200, default 50 |
| `offset` | i64 | no | 0+, default 0 |

### `task_memory_refs`

| Param | Type | Required | Validation |
|-------|------|----------|------------|
| `id` | String | yes | must resolve to existing task |

---

## Removed Task Tools

| Tool | Replacement |
|------|-------------|
| `task_parent_get` | Read `epic_id` from task object → call `epic_show` |
| `task_children_list` | `epic_tasks` |

---

## Summary

| Category | Count |
|----------|-------|
| New epic tools | 9 |
| Modified task tools | 6 |
| Removed task tools | 2 |
| Unchanged task tools (gain validation) | 9 |
| **Net new MCP methods** | **+7** |

## Relations
- [[decisions/adr-003-split-epic-and-task-mcp-tools-with-input-validation|ADR-003: Split Epic and Task MCP Tools with Input Validation]] — decision record
- [[requirements/v1-requirements]] — KANBAN and ROAD views consume these tools
