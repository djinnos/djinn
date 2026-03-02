---
name: plan-milestone
description: Decompose a milestone into tasks on the Djinn board. Creates features and tasks with wave ordering, acceptance criteria, and full traceability.
---

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

Read the planning artifacts needed to decompose this milestone. Execute these sub-steps in order, building a structured context map that all subsequent steps will reference.

1. **Read the roadmap:** `memory_read(identifier="roadmap")`
   - Parse the content to find milestone/phase N (matching `$ARGUMENTS`)
   - Extract three things: the milestone's **goal** (outcome statement), **success_criteria[]** (testable statements), and **req_ids[]** (the REQ-IDs listed in the milestone's Requirements field, e.g., `PLAN-02, PLAN-03, PLAN-04`)
   - **If the roadmap note does not exist:** STOP the workflow with the error: "Cannot plan milestone {N}: no roadmap found in Djinn memory. Run `/djinn:new-project` first to create a roadmap." Do not proceed.

2. **Load requirements:** `memory_search(type="requirement")`
   - From the returned requirement notes, match the REQ-IDs extracted in sub-step 1
   - For each matched requirement, extract its description and domain category (the prefix before the dash, e.g., "PLAN" from "PLAN-02")
   - Store the requirements permalink (e.g., `requirements/v1-requirements`) for use in `memory_refs` later

3. **Catalog existing research:** `memory_search(type="research")`
   - Build a list of existing research notes and the domain topics they cover (based on title, tags, and content summary)
   - This catalog is used in Step 2 to determine which domains need additional research
   - Do not read the full content of every research note yet -- just catalog what exists

4. **Load ADRs:** `memory_search(type="adr")`
   - Filter results for ADRs relevant to this milestone's domain areas (match by tags, title keywords, or wikilinks referencing this milestone's requirements)
   - Read the full content of relevant ADRs -- these contain architectural decisions that constrain task decomposition

5. **Load scope notes from discuss-milestone:** `memory_search(query="milestone {N} scope", type="reference")`
   - Look for a scope reference note created by the discuss-milestone workflow (titled "Milestone {N} Scope")
   - **If no scope notes found:** Display the warning: "No discussion context found for milestone {N} -- planning with defaults." Proceed with available context. This is normal if discuss-milestone was not run before planning.

6. **Check existing epics on the task board:** `task_list(issue_type="epic", project=PROJECT)`
   - Retrieve all existing epics to avoid creating duplicates (per ADR-001, epics are domain-structured and persist across milestones)
   - Record each epic's ID, title, and domain area for matching in Step 3

7. **Build the context summary.** Assemble a structured working context from all sub-steps above:
   ```
   {
     goal: "...",                    // from sub-step 1
     success_criteria: [...],        // from sub-step 1
     req_ids: [...],                 // from sub-step 1
     requirements: [...],            // from sub-step 2 (matched requirement details)
     research_topics: [...],         // from sub-step 3 (existing research catalog)
     adrs: [...],                    // from sub-step 4 (relevant ADR content)
     scope_preferences: [...],       // from sub-step 5 (may be empty)
     existing_epics: [...]           // from sub-step 6 (ID + title pairs)
   }
   ```
   This context map is the planner's working memory for all subsequent steps.

**Note on `memory_build_context`:** You may optionally call `memory_build_context(url="roadmap", depth=2, max_related=10)` to supplement the context above by traversing wikilinks from the roadmap. However, this should NOT replace the targeted reads in sub-steps 1-6. Broad context building risks pulling in the entire knowledge graph and consuming context window budget before task creation begins. Use it only if specific notes referenced via wikilinks were missed by the targeted searches.

**Self-sufficiency:** If the roadmap note is missing, the workflow cannot proceed (see sub-step 1). But if other notes are missing -- no requirements, no research, no ADRs, no scope notes -- proceed with what exists and log what was not found. The workflow is designed to work with partial context, producing the best plan possible from available information.

### Step 2: Domain Research (gap-triggered)

If the milestone involves domain areas with no existing research coverage, investigate those gaps inline before decomposing into tasks. This step runs only when needed -- if existing research already covers all relevant domains, skip it entirely.

1. **Extract domain areas** from the milestone's goal and requirements (from Step 1's context summary). For example, a milestone about "core planning" touches domains like "task decomposition", "wave ordering", "plan validation". Use the requirement category prefixes (e.g., "PLAN", "TRACE") and the milestone goal's key concepts as domain identifiers.

2. **Check research coverage** for each domain area against Step 1's research catalog (`research_topics[]`). A domain is "covered" if an existing research note's title, tags, or content summary addresses that topic.

3. **For domains with no research coverage:**

   a. **Search the project codebase first** for existing patterns. Use Grep and Glob to find relevant files, imports, configurations, and established conventions related to the uncovered domain. This grounds the research in what already exists in the project.

   b. **If domain knowledge gaps remain** after codebase exploration, use WebSearch to gather current best practices, standard patterns, and recommended approaches for the domain.

   c. **Write findings to Djinn memory:**
      ```
      memory_write(
        type="research",
        title="{Domain} Research - Milestone {N}",
        tags=["research", "{domain}", "milestone-{N}"],
        content="# {Domain} Research - Milestone {N}

        ## Summary
        [Key findings from codebase exploration and web research]

        ## Findings
        [Detailed findings organized by sub-topic]

        ## Recommendations
        [Specific recommendations for task decomposition]

        ## Relations
        - [[Roadmap]] -- Milestone {N}
        - [[V1 Requirements]] -- {relevant REQ-IDs}
        - [[{related research note}]] -- prior research on related topic"
      )
      ```
      See `cookbook/planning-templates.md` for the full research note template.

   d. **Include wikilinks** to `[[Roadmap]]` and the relevant requirement notes in the Relations section so the research note is connected to the knowledge graph.

4. **Re-read newly created research notes** to incorporate their findings into the planning context. Update the context summary's `research_topics[]` with the new coverage.

5. **If ALL domain areas already have research coverage:** Skip this step entirely and log: "Existing research covers all domains for milestone {N} -- skipping domain research."

**Note:** The researcher runs INLINE within this workflow -- it is not a separate agent. It has direct access to the context assembled in Step 1. Spawning a separate agent would lose that context and require re-loading it, adding overhead without benefit. The gap-triggered researcher's job is small and focused: fill specific knowledge gaps, not conduct full-dimension research like the new-project workflow.

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

### Step 6: Plan Validation (4-dimension checker with revision iterations)

Verify the task decomposition achieves the milestone's goals. The plan-checker runs INLINE (not as a separate agent) for direct access to all created task IDs from Steps 3-5.

1. **Collect the full task inventory.** After all tasks are created (Steps 3-5), gather the complete list of created task IDs with their details: `{id, title, parent, labels, acceptance_criteria, memory_refs, blocked_by}`. This is the dataset the checker validates against.

2. **Run up to 3 validation iterations.** For each iteration, check all four dimensions below. If any dimension finds a gap, auto-fix it immediately and log the fix. After checking all four dimensions, evaluate whether any gaps were found:

   **Dimension 1 -- Success criteria coverage:**
   For each milestone success criterion from Step 1's context summary (`success_criteria[]`):
   - Check if any created task's `acceptance_criteria` addresses this criterion
   - If not covered: create a task to cover it using the same `task_create` pattern as Step 4 -- with `parent` set to the most relevant feature, `labels=["wave:N"]` for appropriate wave, `acceptance_criteria` addressing the criterion, `memory_refs` linking to requirements, and `priority` as integer (0=critical, 1=high, 2=medium, 3=low)
   - Log: "Gap found: criterion '{criterion}' not covered -> created task {new_task_id}"

   **Dimension 2 -- Requirement coverage:**
   For each REQ-ID from the milestone's requirements list (`req_ids[]`):
   - Check if any created task has `memory_refs` linking to the requirements note permalink
   - If not covered: find the most relevant existing task and update it with `task_update(id=task_id, memory_refs_add=[requirements_permalink])`. If no existing task is relevant, create a new task to address the requirement
   - Log: "Gap found: {REQ-ID} not linked -> updated/created task {task_id}"

   **Dimension 3 -- Hierarchy integrity:**
   For each created task and feature:
   - Verify `parent` is set: tasks must have a parent feature, features must have a parent epic
   - If an item is orphaned: assign it to the most appropriate parent using `task_update(id=item_id, parent=best_match_id)`
   - Log: "Gap found: orphan task {task_id} -> assigned to feature {feature_id}"

   **Dimension 4 -- Wave ordering sanity:**
   - Verify wave:1 tasks have NO blockers. If a wave:1 task has blockers, either reclassify it to a higher wave number or remove the incorrect blockers
   - Verify every wave:N+1 task (N >= 1) is blocked by at least one wave:N task. If not, add a blocker: `task_blockers_add(id=task_id, blocking_id=appropriate_wave_n_task_id, project=PROJECT)`
   - Verify no dependency cycles exist. For milestone-sized graphs of 8-15 tasks, a simple invariant check is sufficient: wave 1 has no blockers, each subsequent wave references only lower waves. If a cycle is detected, break it by removing the blocker that violates the wave ordering
   - Log: "Gap found: wave:{N} task {task_id} missing blocker -> added blocker on {blocking_task_id}"

3. **Evaluate iteration result.** After checking all 4 dimensions: if no gaps were found in this iteration, validation passes -- break out of the loop and proceed to Step 7.

4. **If gaps were found:** The auto-fixes have already been applied inline during the checks above. Increment the iteration counter and re-validate from dimension 1 to catch any cascading issues introduced by the fixes (e.g., a new gap-filling task might itself be an orphan or missing a wave label).

5. **After 3 iterations with gaps still remaining:** Stop iterating. This is the best-effort case. Collect all remaining uncovered gaps into a list for the output summary in Step 8. Do NOT block the workflow -- proceed to Steps 7-8 and report what could not be covered. Let the user decide next steps.

### Step 7: Bidirectional Linking

Establish traceability connections between memory notes and task board items.

1. **Verify forward links (task -> memory):** Confirm all tasks have `memory_refs` pointing to relevant memory notes (requirements, research, ADRs). If any task is missing `memory_refs`, update it: `task_update(id=task_id, memory_refs_add=[requirements_permalink])`

2. **Create backward links (memory -> task):** Update memory notes to reference task IDs in their Relations section. For each requirement note that has tasks implementing its REQ-IDs:
   ```
   memory_edit(
     identifier="requirements/v1-requirements",
     operation="append",
     section="Relations",
     content="\n- Task {id}: {title} -- implements {REQ-ID}"
   )
   ```
   **Note:** `memory_edit` is the exception to the "Do NOT Use memory write tools" rule listed above -- it is explicitly allowed here for appending backlinks in Relations sections. If the requirements note has no existing `## Relations` section, use `operation="append"` without the `section` parameter to add the backlinks at the end of the note.

3. **Verify the traceability chain** is complete end-to-end: Requirement -> Task -> Feature -> Epic -> Roadmap milestone. Spot-check at least 2-3 tasks to confirm the chain is intact.

See `cookbook/task-templates.md` for bidirectional linking patterns.

### Step 8: Output Summary

Present the planning results to the user in a structured format. Include all six sections below, in order:

1. **Task Board Overview** -- Epics, features, and tasks organized by domain (not by wave). This gives the user a clear picture of what was created and where it lives on the board:
   ```
   Epic: {name} ({emoji})
     Feature: {name} ({task_count} tasks)
       - {task_title} [wave:{N}] ({priority_label})
       - {task_title} [wave:{N}] ({priority_label})
     Feature: {name} ({task_count} tasks)
       - {task_title} [wave:{N}] ({priority_label})
   ```
   Priority labels: 0=critical, 1=high, 2=medium, 3=low.

2. **Wave Ordering Diagram** -- Visual representation of the wave structure showing execution order and dependencies:
   ```
   Wave 1: [{task_a}, {task_b}] (no blockers)
   Wave 2: [{task_c}, {task_d}] (blocked by Wave 1)
   Wave 3: [{task_e}] (blocked by Wave 2)
   ```
   Include the task IDs and short titles for each wave.

3. **Success Criteria Coverage Table** -- Maps each milestone success criterion to the task(s) that address it:
   ```
   | Milestone Success Criterion | Covering Task(s) | Status |
   |----------------------------|-------------------|--------|
   | {criterion from roadmap}   | {task_id}: {title}| Covered |
   | {criterion from roadmap}   | --                | Gap     |
   ```

4. **Requirement Coverage Table** -- Maps each REQ-ID to the task(s) that implement it:
   ```
   | REQ-ID   | Covering Task(s)   | Status  |
   |----------|--------------------|---------|
   | {REQ-ID} | {task_id}: {title} | Covered |
   | {REQ-ID} | --                 | Gap     |
   ```

5. **Validation Summary** -- Report from the plan-checker (Step 6):
   - Number of validation iterations run (out of 3 maximum)
   - Gaps found and auto-fixed (list each fix with its dimension and action taken)
   - Any remaining uncovered items from the best-effort case (if all 3 iterations ran and gaps remain)

6. **Missing Context Notice** (conditional) -- Only include this section if Step 1 found missing notes:
   - Display: "Warning: No discussion context found -- planning with defaults"
   - List which notes were missing (e.g., "No ADRs found", "No scope notes from discuss-milestone", "No research notes found")
   - This helps the user understand what context was unavailable and whether they should run discuss-milestone before the next milestone planning

## Output Summary

After a successful run, the following artifacts exist:

**On the task board:**
- Features under domain-structured epics, each with design and acceptance criteria
- Tasks under features, scoped to one-commit outcomes with `{criterion, met}` acceptance criteria
- Wave labels (wave:1, wave:2, wave:3+) on all tasks
- Blocker dependencies enforcing wave ordering via `task_blockers_add`
- `memory_refs` on all tasks linking to requirements, research, and ADR notes

**Traceability:**
- Every milestone success criterion maps to at least one task (verified by plan-checker dimension 1)
- Every REQ-ID for this milestone maps to at least one task (verified by plan-checker dimension 2)
- Bidirectional links between memory notes and task board items (established in Step 7)
- No orphaned tasks -- complete hierarchy from epic to task (verified by plan-checker dimension 3)

**Validation:**
- Plan-checker has run up to 3 iterations across 4 dimensions with auto-fix
- No dependency cycles in the blocker graph (verified by plan-checker dimension 4)
- All tasks are one-commit scope
- Any remaining uncovered gaps are reported in the output summary for user review
