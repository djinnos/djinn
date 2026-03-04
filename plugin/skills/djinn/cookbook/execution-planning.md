# Execution Planning Cookbook

How to run and operate Djinn execution using the current execution APIs.

## Execution Model

Djinn execution dispatches ready tasks from the board to agent sessions. It does not use phase-planning tools.

Core flow:

```
Ready tasks on board
  -> execution_start()
  -> agent sessions run in parallel
  -> task and epic review transitions
  -> execution_status() / session inspection
```

## Core Control Commands

### Start execution

```
execution_start(project=PROJECT)
```

Enables coordinator dispatch for ready tasks.

### Pause execution

```
# Graceful: stop dispatching new tasks, let active sessions finish
execution_pause(mode="graceful")

# Immediate: interrupt active sessions and commit WIP safely
execution_pause(mode="immediate", reason="Emergency stop")
```

### Resume execution

```
execution_resume(project=PROJECT)
```

### Check execution status

```
execution_status(project=PROJECT)
```

Returns coordinator state, active session count, model capacity, and per-task runtime details.

### Kill one running task session

```
execution_kill_task(project=PROJECT, task_id="task-id")
```

Stops that task's active session, commits WIP, and returns the task to a dispatchable state.

## Session Operations

### Find active session for a task

```
session_for_task(project=PROJECT, task_id="task-id")
```

Returns session ID and worktree path.

### List sessions for one task

```
session_list(project=PROJECT, task_id="task-id")
```

### List all currently active sessions

```
session_active(project=PROJECT)
```

## Board Health and Recovery

### Board health snapshot

```
board_health(project=PROJECT)
```

Shows stale tasks, stuck work, and overall board health signals.

### Reconcile stale or stuck state

```
board_reconcile(project=PROJECT, stale_threshold_hours=24)
```

Heals stale tasks, recovers stuck sessions, triggers overdue reviews, and reconciles execution phases/state.

## Agent Roles in Execution

Use these roles consistently in your workflow and status expectations:

- `worker`: implements tasks
- `task-reviewer`: validates task-level correctness
- `epic-reviewer`: validates epic-level integration/completeness
- `conflict-resolver`: handles merge/review conflicts and blocked review flow

## Status Transition Naming

Use epic review terminology (not phase review):

```
open -> in_progress -> needs_task_review -> needs_epic_review -> closed
```

Key actions include:

- `submit_task_review`
- `task_review_approve`
- `epic_review_start`
- `epic_review_approve`
- `epic_review_reject`

## Runtime Settings

Use settings APIs to control model priority and parallelism.

### Read current settings

```
settings_get(key="settings.raw")
```

### Update settings payload

```
settings_set(raw='{"max_sessions":4,"model_priority":["openai/gpt-5.3-codex","anthropic/claude-sonnet-4.5"]}')
```

Typical fields to tune:

- `max_sessions`: upper bound for parallel active sessions
- `model_priority`: ordered model preference list for dispatch

## Practical Operating Workflow

```
# 1) Check board readiness
task_ready(project=PROJECT)

# 2) Start coordinator
execution_start(project=PROJECT)

# 3) Monitor runtime and sessions
execution_status(project=PROJECT)
session_active(project=PROJECT)

# 4) If needed, inspect/interrupt one task session
session_for_task(project=PROJECT, task_id="task-id")
execution_kill_task(project=PROJECT, task_id="task-id")

# 5) Run health checks and reconciliation
board_health(project=PROJECT)
board_reconcile(project=PROJECT)
```

## Common Mistakes

- Using non-existent phase APIs (`execution_phase_*`, `execution_launch_explicit`, `step_diff`)
- Referring to `phase_review` states/actions instead of `epic_review`
- Forgetting to pass `project` when using project-scoped board/execution/session tools
- Pausing immediately without a reason when you need auditability
