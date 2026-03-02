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
