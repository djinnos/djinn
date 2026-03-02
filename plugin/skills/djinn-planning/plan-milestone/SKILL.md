# Plan Milestone Workflow

Decompose a specific milestone from the roadmap into domain-structured Djinn tasks with acceptance criteria, design context, and wave-based ordering. This workflow reads context from Djinn memory (roadmap, requirements, research), creates features and tasks on the task board under the appropriate epics, validates the plan against the milestone's success criteria, and establishes bidirectional traceability between memory notes and task board items.

## Arguments

`$ARGUMENTS` contains the milestone number to plan (e.g., "2" for Phase 2 of the roadmap).

Parse the milestone number from the router's forwarded arguments. If no number is provided, list available milestones from the roadmap and ask the user which one to plan.

## Tools

| Tool | Purpose |
|------|---------|
| memory_read | Read roadmap, requirements, and research notes from Djinn memory |
| memory_search | Find relevant notes by keyword, type, or tag |
| memory_build_context | Traverse wikilinks from a seed note to assemble full context |
| task_create | Create features and tasks under the milestone's epic |
| task_update | Add design, acceptance_criteria, and memory_refs to existing tasks |
| task_blockers_add | Set wave ordering via blocker dependencies between tasks |
| task_list | Check existing tasks, verify no duplicates, list epics |
| task_children_list | List children of an epic to see existing features and tasks |

## Do NOT Use

These tools are outside this workflow's scope:

- **Execution tools** (execution_*, session_for_task): Owned by the Djinn coordinator for running agents during execution. Planning workflows never manage execution sessions.
- **Task lifecycle tools** (task_claim, task_transition): For agents claiming and completing work during execution. Planning creates tasks in `open` status and leaves lifecycle to the coordinator.
- **Memory write tools** (memory_write, memory_edit): This workflow reads from memory to inform planning. It does not create new planning artifacts (the brief, research, requirements, and roadmap already exist from new-project). Exception: optionally writing research notes from the domain researcher agent in Step 2.
- **Sync tools** (task_sync_*): Infrastructure concern for syncing external systems. Not relevant to planning workflows.
- **Settings/provider tools** (settings_*, provider_*): Admin and model management functions. Planning workflows do not configure the system.

## Workflow Steps

### Step 1: Load Context from Memory

Read the planning artifacts needed to decompose this milestone.

1. Read the roadmap: `memory_read(identifier="roadmap")`
2. Find the target milestone/phase by number from `$ARGUMENTS`
3. Extract the milestone's goal, requirements list (REQ-IDs), and success criteria
4. Read requirements: `memory_search(query="{requirement_ids}", type="requirement")`
5. Read relevant research: `memory_search(query="{milestone_topic}", type="research")`
6. Build full context: `memory_build_context(url="roadmap", depth=2)` to traverse wikilinks and gather related notes
7. If the milestone references specific ADRs or design notes, read those too

At the end of this step, you should have a clear understanding of:
- What the milestone must achieve (success criteria)
- Which requirements it addresses (REQ-IDs)
- What technical context exists from research and ADRs

`[Phase 3 implements the full context loading here]`

### Step 2: Domain Research (optional)

If the milestone involves unfamiliar domain concepts, spawn a researcher agent to gather context.

1. Identify knowledge gaps -- areas where the research notes lack sufficient detail for task decomposition
2. Spawn a researcher to investigate the gap
3. Researcher writes findings to memory: `memory_write(type="research", tags=["research", "{domain}"])`
4. Read the new research note to incorporate findings into the plan

See `cookbook/planning-templates.md` for research note template.

`[Phase 3 implements the researcher agent here]`

### Step 3: Identify Epic and Features

Determine where on the task board this milestone's work belongs.

1. Check for existing epics: `task_list(issue_type="epic")`
2. Match the milestone's domain areas to existing epics
3. If an epic exists for a domain area, use it. If not, create via `task_create(issue_type="epic", ...)`
4. List existing features under each epic: `task_children_list(id="{epic_id}")`
5. Determine which features need to be created for this milestone
6. Avoid duplicating features that already exist from prior milestone planning

Per ADR-001, epics are domain concepts (not milestone names). A single milestone may create tasks under multiple epics, and a single epic may receive tasks from multiple milestones.

See `cookbook/task-templates.md` for epic and feature creation patterns.

### Step 4: Decompose into Tasks

Create features and tasks with full detail for each piece of the milestone's work.

For each feature, create it with:
- `description`: What this feature accomplishes and its scope
- `design`: Implementation approach, ADR references, key technical decisions
- `acceptance_criteria`: Array of testable criteria derived from the milestone's success criteria
- `memory_refs`: Links to relevant memory notes (requirements, research, ADRs)

For each task under a feature, create it with:
- `description`: One-commit-sized scope of work
- `design`: Specific implementation steps and technical approach
- `acceptance_criteria`: Array of `{criterion, met}` objects -- `met` starts as `false`
- `priority`: Based on dependency ordering (`critical` > `high` > `medium` > `low`)
- `labels`: `["wave:N"]` for wave-based ordering
- `memory_refs`: Links to the requirements this task implements

See `cookbook/task-templates.md` for task creation and wave patterns.

### Step 5: Set Wave Ordering

Assign wave labels and set blocker dependencies to control execution order.

1. **Wave 1 tasks**: No blockers. These are foundation tasks that can start immediately (schemas, configs, interfaces).
2. **Wave 2 tasks**: `blocked_by` one or more Wave 1 task IDs. Core logic that depends on foundation.
3. **Wave 3 tasks**: `blocked_by` one or more Wave 2 task IDs. Integration and validation that depends on core logic.
4. Use `task_blockers_add()` to add blocker relationships beyond those set at creation time.

**Key principle**: Only block on real technical or logical dependencies. If two tasks CAN run in parallel, do not create an artificial blocker between them. Let the Djinn coordinator parallelize everything that is not explicitly blocked.

See `cookbook/task-templates.md` for wave ordering examples.

### Step 6: Plan Validation

Verify the task decomposition achieves the milestone's goals.

1. **Success criteria coverage**: Check that every milestone success criterion has at least one task whose acceptance criteria addresses it
2. **Requirement coverage**: Verify every REQ-ID assigned to this milestone has at least one task with a `memory_refs` link to the requirements note
3. **Hierarchy integrity**: Confirm no orphaned tasks (all tasks have a parent feature, all features have a parent epic)
4. **Wave ordering sanity**: Verify no dependency cycles in the blocker graph. Wave 1 tasks must have no blockers.
5. **Scope check**: Ensure no tasks exceed one-commit scope. Split oversized tasks into smaller units.

If validation finds gaps, create additional tasks or adjust existing ones. Repeat validation after fixes.

`[Phase 3 implements the plan-checker with 3 revision iterations here]`

### Step 7: Bidirectional Linking

Establish traceability connections between memory notes and task board items.

1. Verify all tasks have `memory_refs` pointing to relevant memory notes (requirements, research, ADRs)
2. Optionally update memory notes to reference task IDs in their Relations section:
   ```
   memory_edit(identifier="requirements/v1-requirements", operation="append",
     section="Relations", content="\n- Task {id}: {title} -- implements {REQ-ID}")
   ```
3. Verify that the link chain is traceable: Requirement -> Task -> Feature -> Epic -> Roadmap milestone

See `cookbook/task-templates.md` for bidirectional linking patterns.

### Step 8: Output Summary

Present the planning results to the user.

1. **Features and tasks created**: List all new features and tasks organized by epic
2. **Wave ordering diagram**: Show waves with dependencies between them
   ```
   Wave 1: [task-a, task-b] (no blockers)
   Wave 2: [task-c, task-d] (blocked by Wave 1)
   Wave 3: [task-e] (blocked by Wave 2)
   ```
3. **Success criteria mapping**: Table showing each milestone success criterion and the tasks that address it
4. **Requirement coverage**: Table showing each REQ-ID and the tasks implementing it
5. **Uncovered items**: Report any success criteria or requirements that could not be mapped to tasks
6. **Scope notes**: Any decisions made about scope boundaries during decomposition

## Output Summary

After a successful run, the following artifacts exist:

**On the task board:**
- Features under domain-structured epics, each with design and acceptance criteria
- Tasks under features, scoped to one-commit outcomes
- Wave labels (wave:1, wave:2, wave:3) on all tasks
- Blocker dependencies enforcing wave ordering
- memory_refs on all tasks linking to requirements, research, and ADR notes

**Traceability:**
- Every milestone success criterion maps to at least one task
- Every REQ-ID for this milestone maps to at least one task
- Bidirectional links between memory notes and task board items
- No orphaned tasks -- complete hierarchy from epic to task

**Validation:**
- No dependency cycles in the blocker graph
- All tasks are one-commit scope
- Plan-checker has verified coverage (when Phase 3 implements it)

---

## Reference: Phase 3 Extension Points

Phase 3 will implement the full methodology for these areas. The markers below identify where implementation logic will be added:

| Step | Extension Point | What Phase 3 Adds |
|------|----------------|-------------------|
| Step 1 | `[Phase 3 implements the full context loading here]` | Structured context assembly with wikilink traversal and relevance filtering |
| Step 2 | `[Phase 3 implements the researcher agent here]` | Domain research agent spawning with memory-write capability |
| Step 6 | `[Phase 3 implements the plan-checker with 3 revision iterations here]` | Automated plan validation with up to 3 revision loops before escalating |
