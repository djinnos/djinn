---
name: djinn
description: Use when djinn MCP tools are available and user needs to manage tasks, store knowledge in memory, or run parallel agent execution. Triggers on task creation, board updates, memory writes, execution launches, or mentions of djinn, kanban, epics, roadmap.
---

# Djinn

Djinn gives you three systems: **tasks** (kanban board), **memory** (persistent knowledge base), **execution** (parallel agent orchestration). This skill is workflow-agnostic -- use it your way.

For project planning workflows, use the dedicated skills: `new-project`, `discuss-milestone`, `plan-milestone`, `progress`.

## Session Start

Always orient first:

1. `memory_catalog()` — see what knowledge exists
2. `task_list(project=..., status="in_progress")` — check active work
3. `task_ready(project=..., issue_type="!epic")` — see what's next

## Task Hierarchy

```
epic (weeks+) → feature (2-4h) → task (1 outcome) / bug (defect)
```

Always set `parent`. Always set `issue_type`. Use `acceptance_criteria` (array), not description, for what "done" looks like. Use `design` for how to implement.

## Status Transitions

```
open → in_progress → needs_task_review → needs_phase_review → closed
```

Key actions: `start`, `submit_task_review`, `task_review_approve`, `phase_review_approve`, `close` (skip review), `reopen`, `block`/`unblock`.

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
| Phase planning, launch, monitor, review | `cookbook/execution-planning.md` |
| Structuring epics → features → tasks | `cookbook/work-decomposition.md` |

## Common Mistakes

| Mistake | Fix |
|---------|-----|
| Putting acceptance criteria in `description` | Use the `acceptance_criteria` array field — description is for context/background only |
| Creating memory without searching first | Always run `memory_catalog()` or `memory_search()` before writing — avoid duplicates |
| Skipping `memory_catalog()` at session start | Run it first — it tells you what knowledge exists before you create or search |
| Setting blockers on features that could run in parallel | Only block on real technical or logical dependencies — let the coordinator parallelize the rest |
| Using `close` on a task that needs review | Use `submit_task_review` → let the review pipeline run. `close` skips review entirely. |
| Omitting `project` path | Most tools require `project` — use the absolute path to the project directory |
| Creating tasks without `parent` | Every non-epic needs a parent ID. Tasks without parents are orphaned from the hierarchy. |
