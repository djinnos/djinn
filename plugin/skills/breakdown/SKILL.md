---
name: breakdown
description: Break planned work into tasks on the Djinn board. Reads plan context from memory, creates tasks under epics with acceptance criteria, design, and blocker-based ordering.
---

# Breakdown Workflow

The mechanical counterpart to `/plan`. Reads the plan context (roadmap, ADRs, scope notes, requirements) and creates tasks on the Djinn board under the appropriate epics. No feature-to-task decomposition -- just creates tasks directly as flat items under epics, sequenced with blockers.

## Tools

| Tool | Purpose |
|------|---------|
| memory_read | Read roadmap, requirements, ADRs, scope notes |
| memory_search | Find relevant notes by keyword, type, or tag |
| memory_build_context | Traverse wikilinks from a seed note for full context |
| memory_edit | Append backlinks to memory notes (Relations sections) |
| task_create | Create tasks under epics with `blocked_by` for atomic dependency setting |
| task_update | Update tasks -- add/remove memory_refs, blocked_by, change fields after creation |
| task_list | Check existing tasks, verify no duplicates |
| epic_create | Create epics if they don't exist |
| epic_list | List existing epics to avoid duplicates |
| epic_tasks | List children of an epic to see existing tasks |

## Do NOT Use

- **Execution tools** (execution_*, session_for_task): Owned by the coordinator.
- **Task lifecycle tools** (task_claim, task_transition): For execution, not planning.
- **Memory write tools** (memory_write): Breakdown reads from memory, it does not create planning artifacts. Exception: `memory_edit` for appending backlinks.
- **Sync tools** (task_sync_*): Infrastructure concern.
- **Settings/provider tools** (settings_*, provider_*): Admin functions.

## Critical API Facts

**All task types are flat siblings under an epic.** Features, tasks, and bugs created via `task_create` share the same level -- there is no parent-child relationship between them. Do NOT create a feature and then try to decompose it into child tasks. Just create tasks directly.

**`epic_id` is optional.** Tasks can be standalone or epic-linked.

## Workflow Steps

### Step 1: Load Context from Memory

Read the planning artifacts needed to create tasks.

1. **Read the roadmap:** `memory_read(identifier="roadmap")`
   - If no roadmap exists: STOP with "No roadmap found. Run `/init-project` first."

2. **Determine what to break down.** Check for planned but unbroken-down work:
   - Search for scope notes: `memory_search(type="reference", query="scope")`
   - Check existing tasks: `task_list(project=PROJECT)` and `epic_tasks(project=PROJECT, epic_id=...)` for each epic
   - Identify phases/areas that have scope notes but no tasks yet
   - If multiple areas are ready, ask the user which to break down
   - If nothing is ready, tell the user to run `/plan` first

3. **Load requirements:** `memory_search(type="requirement")`
   - Match REQ-IDs for the target phase/area
   - Store the requirements permalink for `memory_refs_add` later

4. **Load ADRs:** `memory_search(type="adr")`
   - Filter for ADRs relevant to this area (by tags, title, or wikilinks)
   - These contain decisions that constrain task design

5. **Load scope notes:** Read the scope note for the target area
   - Extract In Scope items (these become tasks)
   - Extract Preferences (these inform task design fields)

6. **Check existing epics:** `epic_list(project=PROJECT)`
   - Record each epic's ID and title for matching

### Step 2: Domain Research (gap-triggered)

If the work involves domain areas with no existing research, investigate gaps before creating tasks.

1. Extract domain areas from the scope and requirements
2. Check research coverage: `memory_search(type="research")`
3. For uncovered domains:
   - Search the project codebase first (Grep, Glob) for existing patterns
   - If gaps remain, use WebSearch for best practices
   - Write findings: `memory_write(type="research", title="{Domain} Research", ...)`
4. If all domains are covered, skip this step

### Step 3: Create Tasks In Dependency Order

**CRITICAL: The coordinator dispatches any open task with no unresolved blockers IMMEDIATELY.** You must create tasks in dependency order -- foundation first, then downstream -- and use `blocked_by` on `task_create` to set blockers atomically at creation time.

**Creation sequence:**
1. Create all foundation tasks first (no blockers needed -- these are the starting points)
2. For each downstream task: pass `blocked_by` with the IDs of its upstream dependencies
3. Always create upstream tasks before downstream tasks so their IDs exist

**Task creation pattern:**
```
# 1. Create foundation task (no blockers)
task_create(
  project=PROJECT,
  title="{foundation deliverable}",
  issue_type="task",
  epic_id="{matching_epic_id}",
  description="{what this accomplishes and its scope}",
  design="{implementation approach, ADR references, technical decisions}",
  acceptance_criteria=[
    {"criterion": "{code-testable condition}", "met": false},
    {"criterion": "{code-testable condition}", "met": false}
  ],
  priority=0,
  memory_refs=["{requirements_permalink}", "{adr_permalink}"]
)
# Returns: { id: "a1b2" }

# 2. Create downstream task with blockers set atomically
task_create(
  project=PROJECT,
  title="{downstream deliverable}",
  issue_type="task",
  epic_id="{matching_epic_id}",
  description="...",
  design="...",
  acceptance_criteria=[...],
  priority=1,
  memory_refs=[...],
  blocked_by=["a1b2"]
)
# Returns: { id: "c3d4" } -- created already blocked, never dispatched prematurely
```

**Use `issue_type="feature"` when** the item is a user-facing deliverable (login UI, search page, onboarding flow). **Use `issue_type="task"` when** the item is internal implementation work (database schema, middleware, config). They are peers -- do NOT decompose features into sub-tasks.

**Sizing:** Each task should be completable in one focused agent session. If something is too large, split it into multiple independent tasks at the same level.

**Priority values (integer, 0=highest):**
- `0` = Critical (foundation, must go first)
- `1` = High (core logic)
- `2` = Medium (integration)
- `3` = Low (nice-to-have)

**Acceptance criteria must be code-testable.** Every criterion must be verifiable by an agent through unit tests, integration tests, or code inspection. Never use:
- Production metrics ("20 transactions processed per second")
- Manual verification ("user confirms the flow feels right")
- Environment-dependent checks ("works in staging")

Good: "POST /api/login returns 401 with invalid credentials"
Good: "Unit test verifies JWT token contains user_id claim"
Bad: "System handles 1000 concurrent users in production"
Bad: "Stakeholder approves the design"

### Step 4: Validate

Verify the task decomposition covers the plan.

Run up to 3 validation iterations. For each iteration, check all dimensions:

**Dimension 1 -- Success criteria coverage:**
For each milestone success criterion from the roadmap:
- Check if any task's `acceptance_criteria` addresses it
- If not: create a task to cover it

**Dimension 2 -- Requirement coverage:**
For each REQ-ID in scope:
- Check if any task has `memory_refs` linking to the requirements note
- If not: update the most relevant task with `task_update(memory_refs_add=...)`

**Dimension 3 -- Epic linkage:**
- Verify tasks are grouped under appropriate epics
- If ungrouped tasks should be epic-linked: `task_update(id=..., epic_id=...)`

**Dimension 4 -- Dependency ordering:**
- Foundation tasks have no blockers
- Downstream tasks are blocked by prerequisites
- No dependency cycles

After 3 iterations with remaining gaps, report them and proceed.

### Step 5: Bidirectional Linking

Establish traceability between memory and tasks.

1. **Forward links (task -> memory):** Already done via `memory_refs` on `task_create` in Step 3
2. **Backward links (memory -> task):** Update memory notes to reference tasks:
   ```
   memory_edit(
     identifier="requirements/v1-requirements",
     operation="append",
     section="Relations",
     content="\n- Task {id}: {title} -- implements {REQ-ID}"
   )
   ```

### Step 6: Summary

Present results:

1. **Task Board Overview** -- Tasks organized by epic:
   ```
   Epic: {name}
     - {task_title} (priority {N})
     - {task_title} (priority {N})
   ```

2. **Execution Order** -- Visual dependency stages:
   ```
   Stage 1: [{task_a}, {task_b}] (no blockers)
   Stage 2: [{task_c}] (blocked by Stage 1)
   Stage 3: [{task_d}] (blocked by Stage 2)
   ```

3. **Coverage Tables** -- Success criteria and requirement coverage

4. **Validation Summary** -- Gaps found and fixed, remaining issues

## Output Summary

After a successful run:

**On the task board:**
- Tasks under domain-structured epics with design and acceptance criteria
- Blocker dependencies enforcing execution order
- memory_refs on all tasks linking to requirements and ADRs

**Traceability:**
- Every success criterion maps to at least one task
- Every in-scope REQ-ID maps to at least one task
- Bidirectional links between memory notes and tasks
