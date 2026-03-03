# Execution Planning Cookbook

How to plan, launch, and monitor automated agent execution in djinn.

## What Execution Does

Execution is djinn's automation layer. It:
1. Groups tasks into **phases** (tasks that can run in parallel)
2. Respects **dependencies** — blocked tasks wait for their blockers
3. Spawns **AI agents** in isolated git worktrees, one per task
4. Gates on **review** before merging

Think of it as: "take these ready tasks, figure out a parallel execution plan, and run it."

## The Execution Model

```
Tasks (on the board)
    ↓
Phases (planned execution groups)
    ↓
Agents (one per task, parallel within a phase)
    ↓
Review (AI review + your approval)
    ↓
Merge (changes land on main)
```

Each phase gets a **feature branch**. Tasks within a phase share that branch. Phases with no dependencies between them can run in parallel.

## Phase Lifecycle

```
waiting → active → reviewing → completed
```

- **waiting**: Queued, not yet executing
- **active**: Agents are working on tasks
- **reviewing**: Tasks submitted for review
- **completed**: Phase merged and done

## Planning Execution

### Preview what djinn would plan
```
execution_preview_unified(project="/path/to/project")
```

Returns:
- `existing_phases` — phases already in the plan
- `suggested_phases` — what djinn recommends for unassigned tasks
- `unassigned_tasks` — tasks not yet in any phase

Use this to understand the plan before committing to it.

### Launch with djinn's recommended plan
```
execution_start(project="/path/to/project")
```

Djinn auto-groups tasks by dependencies, creates phases, and starts execution.

### Launch with an explicit custom plan

When you want fine-grained control:

```
execution_launch_explicit(
  project="/path/to/project",
  phases_to_create=[
    {
      "name": "Phase 1: Auth Foundation",
      "branch_name": "feature/auth-foundation",
      "task_ids": ["task-1", "task-2"]
    },
    {
      "name": "Phase 2: Auth UI",
      "branch_name": "feature/auth-ui",
      "task_ids": ["task-3", "task-4"]
    }
  ],
  phases_to_resume=["existing-phase-id"],
  phases_to_defer=["phase-to-skip-id"],
  auto_merge=false
)
```

### Set auto-merge behavior
```
# Auto-merge approved phases (use with caution)
execution_start(project="...", auto_merge=true)

# Manual review required for every phase (default)
execution_start(project="...", auto_merge=false)
```

## Monitoring Execution

### Check overall status
```
execution_status(project="/path/to/project")
```

### List tasks under an epic (phase equivalent)
```
epic_tasks(epic_id="epic-id")
```

Shows: tasks under an epic with status, priority, and labels.

### Get the code diff for a phase
```
step_diff(phase_id="phase-uuid", project="/path/to/project")
```

Returns: files changed, additions/deletions, per-task attribution.

## Managing Phases

### Create a new phase
```
execution_phase_create(
  branch_name="feature/my-feature",
  name="My Feature Phase",
  task_ids=["task-1", "task-2"],
  depends_on=["other-phase-id"]  # Optional: wait for another phase
)
```

### Add a task to an existing phase

```
# Add to the currently active phase (requires confirmation)
execution_phase_add_task(task_id="task-id", target="active_phase", confirmed=true)

# Add to the next waiting phase (safe default)
execution_phase_add_task(task_id="task-id", target="waiting_phase")

# Add to a specific phase by ID
execution_phase_add_task(task_id="task-id", target="phase-uuid")
```

### Move a task between phases
```
execution_phase_move_task(
  task_id="task-id",
  source_phase_id="phase-a-id",
  target_phase_id="phase-b-id"
)
```

### Remove a task from a phase
```
execution_phase_remove_task(task_id="task-id")
```

### Update phase name or branch
```
execution_phase_update(
  phase_id="phase-uuid",
  name="Better Phase Name",
  branch_name="feature/better-name"  # Only if not yet provisioned
)
```

### Close a completed phase
```
execution_phase_close(phase_id="phase-uuid", project="...")
```

All tasks in the phase must be in terminal status first.

### Reopen a completed phase (for follow-up work)
```
execution_reopen_phase(phase_id="phase-uuid", project="...")
```

### Create a follow-up phase stacked on a completed one
```
execution_create_followup(
  completed_phase_id="phase-uuid",
  project="..."
)
```

Creates a new phase based on the completed phase's branch — useful for layered work.

### Delete a phase
```
execution_phase_delete(phase_id="phase-uuid")

# Force delete even if tasks are in_progress
execution_phase_delete(phase_id="phase-uuid", force=true)
```

Note: Git branches are NOT deleted for safety.

## Pausing and Resuming

### Pause gracefully (let active tasks finish)
```
execution_pause(mode="graceful", reason="User requested pause")
```

Stops new task dispatch. Active sessions run to completion.

### Pause immediately (commit WIP)
```
execution_pause(mode="immediate", reason="Emergency stop")
```

Commits work-in-progress to each task's branch. Interrupted tasks return to `open` status.

### Resume
```
execution_resume()
```

### Kill a specific task's session
```
execution_kill_task(task_id="task-id", project="...")
```

Commits WIP, releases worktree, task returns to open status.

## Applying Batch Changes

For atomic multi-change operations:

```
execution_apply_changes(
  project="...",
  task_moves=[
    {"task_id": "task-1", "target_phase_id": "phase-b"}
  ],
  new_phases=[
    {"name": "New Phase", "branch_name": "feature/new", "task_ids": ["task-5"]}
  ],
  deferrals=["phase-to-defer-id"],
  activations=["phase-to-activate-id"],
  removed_tasks=[
    {"task_id": "task-3", "phase_id": "phase-a"}
  ]
)
```

All mutations are atomic — if any fails, all are rolled back.

## Getting a Session for a Task

Find the active agent session for a running task:
```
session_for_task(task_id="task-id")
```

Returns: session ID and worktree path for the running agent.

## Cleanup

Remove orphaned branches from old plans:
```
# Preview what would be deleted
execution_cleanup_branches(project="...", delete=false)

# Actually delete
execution_cleanup_branches(project="...", delete=true)
```

## Forcing Review

Trigger an immediate architect review for pending tasks:
```
execution_force_review(project="...")
```

## Common Workflows

### Start fresh execution on ready tasks
```
# 1. See what's ready
task_ready()

# 2. Preview the plan
execution_preview_unified(project="...")

# 3. Launch
execution_start(project="...")

# 4. Monitor
execution_status(project="...")
```

### Add urgent task to running execution
```
# 1. Create high-priority task
task_create(title="Hot fix: ...", priority=0, ...)

# 2. Add to active phase (if urgent enough to run now)
execution_phase_add_task(task_id="new-task-id", target="active_phase", confirmed=true)

# OR: Add to waiting phase to run after current finishes
execution_phase_add_task(task_id="new-task-id", target="waiting_phase")
```

### Review and accept a completed phase
```
# 1. See tasks under the epic
epic_tasks(epic_id="epic-id")

# 2. Review the diff
step_diff(phase_id="phase-id", project="...")

# 3. Approve individual tasks
task_transition(id="task-id", action="phase_review_approve", project="...")

# 4. Close the phase when all tasks approved
execution_phase_close(phase_id="phase-id", project="...")
```
