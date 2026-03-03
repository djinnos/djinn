---
name: progress
description: Check project progress by querying the Djinn task board. Shows milestone completion, active waves, blockers, and routes to next action.
---

# Progress Workflow

**Status: v2 -- Scaffold only.** This workflow's full implementation is tracked under PROG-01 and PROG-02 in the v2 requirements. The structural stub exists so the djinn-planning skill directory is complete and the router's file references all resolve.

Query the Djinn task board and execution state to determine project progress and route the user to their next action. All progress is derived from live queries, never stored state -- per ADR-002 (State Derivation), progress comes from the task board itself, not from cached or manually updated state notes.

## Arguments

`$ARGUMENTS` is optional. If provided, it contains a milestone number to check progress for a specific milestone. If omitted, show overall project progress.

## Tools

| Tool | Purpose |
|------|---------|
| memory_read | Read roadmap for milestone context |
| task_list | Query tasks by status, priority, parent |
| task_count | Get aggregate counts grouped by status or priority |
| task_ready | Find tasks ready to start (no blockers) |
| execution_status | Check if execution is running |
| epic_tasks | List tasks under a specific epic |

## Do NOT Use

These tools are outside this workflow's scope:

- **Memory write tools** (memory_write, memory_edit): Progress reads state, it never writes or modifies memory notes
- **Task mutation tools** (task_create, task_update, task_transition, task_claim, task_blockers_add): Progress observes the task board, it never modifies tasks
- **Execution control tools** (execution_start, execution_pause): Progress reports on execution state, it does not start or stop execution
- **Sync tools** (task_sync_push, task_sync_pull, task_sync_status): Infrastructure concern, not workflow
- **Settings tools** (settings_get, settings_update, provider_list, provider_update): Admin functions, not workflow

## Workflow Steps

### Step 1: Query Project State

Gather all the data needed to derive progress from live queries.

1. Read the roadmap from memory: `memory_read("roadmap")` to understand milestone structure
2. Query task counts by status: `task_count(group_by="status")` to see how many tasks are in each state
3. Query ready tasks: `task_ready()` to find tasks with no unresolved blockers
4. Check execution: `execution_status()` to see if an execution session is active
5. If a specific milestone was requested, query its tasks: `epic_tasks(epic_id="{milestone_epic_id}")`

All data comes from live queries. Do not store, cache, or write progress state anywhere.

### Step 2: Derive Progress

Calculate progress from the queried data.

1. **Milestone completion** -- calculate percentage from task status counts (done / total)
2. **Current wave** -- identify which wave's tasks are currently in_progress (the active frontier)
3. **Blocked tasks** -- note any tasks that are blocked and what they are waiting on
4. **Velocity** -- if tasks have completion timestamps, estimate remaining time

All progress is derived from live queries -- never store or cache progress state (per ADR-002: State Derivation).

`[v2 implements the full progress derivation logic with milestone-level and wave-level breakdowns here]`

### Step 3: Route to Next Action

Based on the derived state, suggest what the user should do next.

| Project State | Suggested Action |
|---------------|-----------------|
| All tasks done for milestone | Milestone complete -- suggest planning the next milestone |
| Tasks ready but not started | Suggest starting execution: `execution_start` |
| Tasks blocked | Show blockers and suggest resolving them |
| Execution running | Show current phase progress and active tasks |
| No tasks exist | Suggest running new-project or plan-milestone first |

`[v2 implements the full routing logic with priority-based suggestions and multi-milestone awareness here]`

## Output Summary

After running this workflow, the user sees:

- **Milestone progress** -- percentage complete with task count breakdown
- **Current wave** -- which wave is active and its completion status
- **Blockers** -- any blocked tasks with their dependencies
- **Next action** -- a specific, actionable suggestion based on project state

This workflow does NOT write to memory, create tasks, or modify execution state. It is purely read-only.
