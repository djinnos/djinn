---
name: djinn
description: Use when djinn MCP tools are available and user needs to manage tasks, store knowledge in memory, or run parallel agent execution. Triggers on task creation, board updates, memory writes, execution launches, or mentions of djinn, kanban, epics, roadmap.
---

# Djinn

Djinn gives you three systems: **tasks** (kanban board), **memory** (persistent knowledge base), **execution** (parallel agent orchestration). This skill is workflow-agnostic -- use it your way.

For project planning workflows, use the dedicated skills: `new-project`, `discuss-milestone`, `plan-milestone`, `progress`.

## Session Start

Always orient first:

1. `memory_catalog(project=PROJECT)` — see what knowledge exists
2. `task_list(project=PROJECT, status="in_progress")` — check active work
3. `task_ready(project=PROJECT)` — see what's next

## Hierarchy

Epics and tasks are managed by **separate MCP tools** (per ADR-003):

```
epic_create(project=PROJECT, ...)  → Epic (strategic container, weeks+)
task_create(project=PROJECT, ...)  → feature / task / bug (epic-linked or standalone)
```

Epics use: `epic_create`, `epic_list`, `epic_show`, `epic_tasks`, `epic_update`, `epic_close`, `epic_reopen`, `epic_delete`, `epic_count`.

Tasks use: `task_create(project=PROJECT, issue_type="task"|"feature"|"bug", epic_id=...)` — `epic_id` is optional, so tasks may be standalone or epic-linked. There is no nesting of tasks under features.

Always set `project` and `issue_type`. Use `acceptance_criteria` (array), not description, for what "done" looks like. Use `design` for how to implement.

## Status Transitions

```
open → in_progress → needs_task_review → needs_epic_review → closed
```

Key actions: `start`, `submit_task_review`, `task_review_approve`, `epic_review_approve`, `close` (skip review), `reopen`, `block`/`unblock`.

## Progress Notes

Add comments at milestones so any agent can resume:

- `[STARTING] Approach: ...`
- `[PROGRESS] Done: X. Next: Y.`
- `[BLOCKED] Waiting on: ...`
- `[PAUSED] Left off at: ... Resume by: ...`
- `[DONE] Implemented: ...`

## Memory Types

| Type | Folder | Use for |
|------|--------|---------|
| `adr` | decisions/ | Architectural decisions |
| `pattern` | patterns/ | Reusable code patterns |
| `research` | research/ | Analysis, findings |
| `requirement` | requirements/ | Specs, PRDs |
| `design` | design/ | System designs |
| `brief` | (root) | Project brief (singleton) |
| `roadmap` | (root) | Roadmap (singleton) |

Connect notes with `[[wikilinks]]`. Add `## Relations` section. Search before creating to avoid duplicates.

## Cookbooks

Load when you need detailed patterns:

| Need | Cookbook |
|------|---------|
| Task CRUD, lifecycle, blockers, queries | `cookbook/task-management.md` |
| Memory write, search, wikilinks, maintenance | `cookbook/memory-management.md` |
| Execution control, monitoring, session operations | `cookbook/execution-planning.md` |
| Structuring epics → features → tasks | `cookbook/work-decomposition.md` |

## Common Mistakes

| Mistake | Fix |
|---------|-----|
| Putting acceptance criteria in `description` | Use the `acceptance_criteria` array field — description is for context/background only |
| Creating memory without searching first | Always run `memory_catalog()` or `memory_search()` before writing — avoid duplicates |
| Skipping `memory_catalog()` at session start | Run it first — it tells you what knowledge exists before you create or search |
| Setting blockers on features that could run in parallel | Only block on real technical or logical dependencies — let the coordinator parallelize the rest |
| Using `close` on a task that needs review | Use `submit_task_review` → let the review pipeline run. `close` skips review entirely. |
| Omitting `project` on task/epic tools | `project` is required on task/epic reads and writes (for example `task_create`, `task_list`, `epic_create`, `epic_list`). |
| Assuming `epic_id` is required | `epic_id` is optional. Standalone tasks are valid when epic grouping is not needed. |
| Using `task_create` for epics | Use `epic_create()` — epics have their own tool namespace (ADR-003). |
| Nesting tasks under features | Features, tasks, and bugs are flat siblings under an epic. There is no parent-child between them. |
