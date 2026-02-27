# GSD + Djinn

You are in a GSD project (`.planning/` directory exists). Djinn replaces GSD's execution and verification. GSD still owns planning — you still use `/gsd:new-project`, `/gsd:plan-phase`, `/gsd:discuss-phase`. But when it's time to execute and verify, use djinn.

## The Split

| Phase | Who owns it | What to use |
|-------|-------------|-------------|
| Project setup | GSD | `/gsd:new-project` |
| Phase discussion | GSD | `/gsd:discuss-phase` |
| Phase planning | GSD | `/gsd:plan-phase` — creates PLAN.md files |
| **Phase execution** | **Djinn** | `execution_start` — replaces `/gsd:execute-phase` |
| **Verification** | **Djinn** | Review pipeline — replaces GSD verifier |
| **Research persistence** | **Djinn** | `memory_write` — replaces transient `.planning/` files |
| Milestone completion | GSD | `/gsd:complete-milestone` |

## After plan-phase: Import Plans into Djinn

When `/gsd:plan-phase` finishes, it creates PLAN.md files in `.planning/phases/{N}-{name}/`. Import them into djinn:

**Step 1: Read the plans**
```bash
# Find all plan files for this phase
ls .planning/phases/{N}-{name}/*-PLAN.md
```

Read each PLAN.md. Extract: title, objective, tasks, wave number, dependencies.

**Step 2: Create the epic (if not exists)**
```
task_create(
  title="GSD Milestone: {milestone name}",
  issue_type="epic",
  emoji="🎯",
  project="..."
)
```

**Step 3: Create a feature for the phase**
```
task_create(
  title="Phase {N}: {phase name}",
  issue_type="feature",
  parent=epic.id,
  description="GSD Phase {N}. Goal: {goal from ROADMAP.md}",
  project="..."
)
```

**Step 4: Create a task per plan**

For each PLAN.md:
```
task_create(
  title="{plan objective — first line}",
  issue_type="task",
  parent=phase_feature.id,
  description="{full plan content or key sections}",
  design="{approach from plan}",
  acceptance_criteria=[
    "{extracted from plan's success criteria}"
  ],
  labels=["gsd:phase-{N}", "gsd:plan-{N}-{M}", "gsd:wave-{W}"],
  project="..."
)
```

**Step 5: Set wave dependencies as blockers**

Plans in Wave 2 depend on Wave 1 completing:
```
# For each Wave 2+ task, block on all tasks from previous wave
task_blockers_add(
  id=wave2_task.id,
  blocking_id=wave1_task.id,
  project="..."
)
```

## Execute with Djinn (replaces /gsd:execute-phase)

Once tasks are imported with blockers set:

```
# Preview the execution plan
execution_preview_unified(project="...")

# Launch — djinn handles wave ordering via blockers
execution_start(project="...")

# Monitor
execution_status(project="...")
```

Djinn's coordinator will:
- Spawn agents for ready tasks (Wave 1 first, since Wave 2+ is blocked)
- Run agents in parallel within each wave
- Gate on review before progressing
- Handle failures and retries

**Do NOT run `/gsd:execute-phase`.** Djinn owns execution now.

## Review (replaces GSD verifier)

Djinn's review pipeline replaces GSD's `gsd-verifier`:

```
# List completed phases
execution_phase_list(project="...")

# Review the diff for a phase
step_diff(phase_id="...", project="...")

# Approve tasks individually
task_transition(id="task-id", action="task_review_approve", project="...")

# Approve the phase
task_transition(id="task-id", action="phase_review_approve", project="...")
```

If review fails — task goes back to `in_progress` for rework:
```
task_transition(
  id="task-id",
  action="task_review_reject",
  reason="Missing error handling for edge case X",
  project="..."
)
```

## Save GSD Research to Djinn Memory

GSD creates transient files in `.planning/` that die when context resets. Save the important ones to djinn memory:

**After research phase:**
```
memory_write(
  title="Research: {phase name}",
  type="research",
  content="[content from .planning/phases/{N}-{name}/RESEARCH.md]",
  tags=["gsd", "phase-{N}"]
)
```

**After architectural decisions:**
```
memory_write(
  title="ADR: {decision title}",
  type="adr",
  content="[extracted from CONTEXT.md or plan decisions]",
  tags=["gsd", "adr"]
)
```

**After requirements defined:**
```
memory_write(
  title="Requirements: {project name}",
  type="requirement",
  content="[content from .planning/REQUIREMENTS.md]",
  tags=["gsd", "requirements"]
)
```

## Update GSD State After Execution

After djinn finishes executing a phase, update GSD's tracking files so they stay in sync:

**Update ROADMAP.md:**
```bash
# Mark phase complete in GSD's roadmap
node ~/.claude/get-shit-done/bin/gsd-tools.cjs phase complete "{N}"
```

**Update STATE.md:**
```bash
# Advance GSD state to next phase
node ~/.claude/get-shit-done/bin/gsd-tools.cjs commit "docs(phase-{N}): complete phase execution" \
  --files .planning/ROADMAP.md .planning/STATE.md
```

## Checkpoint Plans (human-in-the-loop)

GSD plans with `autonomous: false` need human interaction. In djinn, handle these by:

1. Creating the task with a label: `labels=["checkpoint"]`
2. When the agent reaches the checkpoint, it adds a comment: `[BLOCKED] Needs human decision: {details}`
3. The agent transitions: `task_transition(action="block", reason="Checkpoint: needs human input")`
4. Human reviews and responds via: `task_comment_add(body="Decision: {choice}")`
5. Agent unblocks and continues: `task_transition(action="unblock")`

## Full Workflow Summary

```
/gsd:new-project          → GSD creates .planning/, REQUIREMENTS.md, ROADMAP.md
  ↓
  Save requirements + roadmap to djinn memory
  ↓
/gsd:discuss-phase {N}    → GSD locks preferences for the phase
  ↓
/gsd:plan-phase {N}       → GSD creates PLAN.md files
  ↓
  Import plans into djinn tasks (this cookbook)
  ↓
execution_start()          → Djinn executes (replaces /gsd:execute-phase)
  ↓
  Review via djinn pipeline (replaces GSD verifier)
  ↓
  Update GSD state files (ROADMAP.md, STATE.md)
  ↓
  Next phase or /gsd:complete-milestone
```
