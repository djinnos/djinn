# Superpowers + Djinn

You have Superpowers skills loaded. Djinn replaces Superpowers' execution skills (`executing-plans`, `subagent-driven-development`). Superpowers still owns brainstorming and plan writing. When it's time to execute, use djinn.

## The Split

| Phase | Who owns it | What to use |
|-------|-------------|-------------|
| Brainstorming | Superpowers | `superpowers:brainstorming` |
| Plan writing | Superpowers | `superpowers:writing-plans` |
| **Plan execution** | **Djinn** | `execution_start` — replaces `executing-plans` and `subagent-driven-development` |
| **Code review** | **Djinn** | Review pipeline — replaces spec-reviewer + code-quality-reviewer subagents |
| **Verification** | **Djinn** | Phase review — replaces `finishing-a-development-branch` |
| Git worktrees | Djinn | Djinn isolates each agent in its own worktree automatically |

## After write-plan: Import Plan into Djinn

When `superpowers:writing-plans` finishes, it creates a plan file (usually in `docs/plans/`). Import it into djinn tasks.

**Step 1: Read the plan, extract tasks**

Read the plan file. Each numbered task becomes a djinn task.

**Step 2: Create a feature for the plan**

```
task_create(
  title="{plan title}",
  issue_type="feature",
  project="...",
  description="{plan overview — what we're building and why}",
  design="{key technical decisions from plan}",
  acceptance_criteria=[
    "{extracted from plan's success criteria or verification steps}"
  ]
)
```

**Step 3: Create a task per plan step**

For each task in the plan:

```
task_create(
  title="{task title from plan}",
  issue_type="task",
  parent=feature.id,
  project="...",
  description="{full task description from plan}",
  design="{implementation approach — what files to touch, patterns to follow}",
  acceptance_criteria=[
    "{what this specific task must achieve}"
  ]
)
```

**Step 4: Set dependencies**

If task B depends on task A completing first:

```
task_blockers_add(id=task_b.id, blocking_id=task_a.id, project="...")
```

Most plan tasks are sequential — each blocks the next.

## Execute with Djinn (replaces executing-plans / subagent-driven-development)

```
execution_preview_unified(project="...")
execution_start(project="...")
```

Djinn's coordinator handles:
- Spawning one agent per task in isolated worktrees
- Respecting blocker ordering (sequential tasks run in order)
- Running independent tasks in parallel
- Review gates between tasks

**Do NOT use `superpowers:executing-plans` or `superpowers:subagent-driven-development`.** Djinn owns execution.

## Review (replaces spec-reviewer + code-quality-reviewer)

Djinn's two-stage review replaces Superpowers' subagent reviewers:

**Stage 1 — Task review** (replaces spec-reviewer):
```
task_transition(id="task-id", action="task_review_approve", project="...")
# OR reject:
task_transition(id="task-id", action="task_review_reject", reason="Missing: ...", project="...")
```

**Stage 2 — Phase review** (replaces code-quality-reviewer):
```
# Review the full diff
step_diff(phase_id="...", project="...")

# Approve
task_transition(id="task-id", action="phase_review_approve", project="...")
# OR reject:
task_transition(id="task-id", action="phase_review_reject", reason="Quality: ...", project="...")
```

Rejected tasks go back to `in_progress` for rework.

## Save Design Decisions to Memory

Superpowers brainstorming produces design docs that disappear with the context window. Persist them:

**After brainstorming:**
```
memory_write(
  title="Design: {feature name}",
  type="design",
  content="{brainstorm output — decisions, trade-offs, chosen approach}",
  tags=["design"]
)
```

**After architectural decisions:**
```
memory_write(
  title="ADR: {decision}",
  type="adr",
  content="{decision, context, consequences}",
  tags=["adr"]
)
```

**After learning something reusable:**
```
memory_write(
  title="Pattern: {name}",
  type="pattern",
  content="{when to use, implementation, gotchas}",
  tags=["pattern"]
)
```

## Finishing (replaces finishing-a-development-branch)

After all tasks in a feature are approved:

1. Check all tasks are closed: `task_list(parent=feature.id, project="...")`
2. Close the feature: `task_transition(id=feature.id, action="close", project="...")`
3. The execution phase handles merging — `execution_phase_close(phase_id="...", project="...")`

## Full Workflow Summary

```
superpowers:brainstorming     → Design doc produced
  ↓
  Save design to djinn memory (memory_write type="design")
  ↓
superpowers:writing-plans     → Plan file produced
  ↓
  Import plan into djinn tasks (this cookbook)
  ↓
execution_start()             → Djinn executes (replaces executing-plans)
  ↓
  Review via djinn pipeline   (replaces spec-reviewer + code-quality-reviewer)
  ↓
  Close feature + phase       (replaces finishing-a-development-branch)
```
