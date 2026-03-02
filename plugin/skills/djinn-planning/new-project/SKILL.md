# New Project Workflow

Guide the user through project definition via deep questioning, then create a complete project in Djinn: a brief, parallel research notes, synthesized research summary, categorized requirements with REQ-IDs, a narrative roadmap in memory, and domain-structured epics and features on the task board. The output is a fully populated knowledge graph and actionable task board ready for milestone planning.

## Tools

| Tool | Purpose |
|------|---------|
| memory_write | Create brief, research, requirements, and roadmap notes in Djinn memory |
| memory_read | Read existing notes for context or to check content before overwriting |
| memory_search | Find notes by keyword to avoid duplicates and discover related content |
| memory_catalog | Orient at session start -- list all existing knowledge in memory |
| task_create | Create domain-structured epics and features on the task board |
| task_blockers_add | Set sequencing dependencies between epics and features |
| task_update | Add memory_refs linking tasks back to memory notes for traceability |

## Do NOT Use

These tools are outside this workflow's scope:

- **Execution tools** (execution_*, session_for_task): Owned by the Djinn coordinator for running agents during execution. Planning workflows never manage execution sessions.
- **Task lifecycle tools** (task_claim, task_transition): For agents claiming and completing work during execution. Planning creates tasks in `open` status and leaves lifecycle to the coordinator.
- **Sync tools** (task_sync_*): Infrastructure concern for syncing external systems. Not relevant to planning workflows.
- **Settings/provider tools** (settings_*, provider_*): Admin and model management functions. Planning workflows do not configure the system.
- **Memory mutation tools** (memory_delete, memory_move): Project setup creates new artifacts. It does not reorganize or delete existing knowledge. Use memory_edit only in the context of appending Relations backlinks.

## Workflow Steps

### Step 1: Orient

Check what already exists in the project before creating anything.

1. Run `memory_catalog()` to list all existing notes in Djinn memory
2. If a brief already exists, confirm with the user before proceeding -- they may want to extend an existing project rather than overwrite it
3. Run `task_list(issue_type="epic")` to check for existing epics on the task board
4. If the project already has artifacts, summarize what exists and ask the user how to proceed

### Step 2: Deep Questioning

Guide the user through project definition with an adaptive, thread-following conversation.

Cover these areas, adapting order and depth based on the user's responses:
- **Vision**: What does the end state look like? What changes for the user?
- **Problem**: What pain point does this solve? What is the current situation?
- **Target users**: Who benefits and how? What are their workflows today?
- **Scope**: What is in scope for v1? What is explicitly out of scope?
- **Constraints**: Technical requirements, timeline, team size, existing systems?
- **Technical preferences**: Language, framework, infrastructure, deployment model?

Follow up on interesting answers. If the user mentions a complex domain, explore it. If they describe a technical constraint, understand the reasoning behind it. Adapt questions based on responses rather than running through a static checklist.

Continue until you have enough context to write a complete project brief. Confirm with the user that the questioning is complete before moving on.

`[Phase 2 implements the full questioning methodology here]`

### Step 3: Write Project Brief

Create the project brief in Djinn memory.

1. Synthesize the questioning session into a structured brief
2. Write to memory: `memory_write(type="brief", content=...)`
3. Include wikilinks in the Relations section pointing to future notes (Requirements, Roadmap)
4. See `cookbook/planning-templates.md` for the brief template and content structure

The brief is a singleton -- only one exists per project. If a brief already exists (detected in Step 1), confirm the overwrite with the user first.

### Step 4: Parallel Research

Spawn research across four dimensions to build a knowledge base for planning.

**Research dimensions:**
1. **Stack** -- technology evaluation, framework selection, infrastructure options
2. **Features** -- feature analysis, UX patterns, competitive landscape
3. **Architecture** -- system design patterns, data models, integration approaches
4. **Pitfalls** -- risks, anti-patterns, common mistakes in this domain

For each dimension:
1. Research the topic using available tools and knowledge
2. Write findings to memory: `memory_write(type="research", tags=["research", "<dimension>"])`
3. Include wikilinks in the Relations section connecting back to the brief and to other research notes

See `cookbook/planning-templates.md` for the research note template.

`[Phase 2 implements the parallel research agent pattern here]`

### Step 5: Research Synthesis

Read all research notes and produce a cross-cutting synthesis.

1. Read all four research dimension notes using `memory_read()` or `memory_search(type="research")`
2. Identify convergent themes across dimensions
3. Identify tensions or conflicts between dimensions and propose resolutions
4. Surface open questions that need to be addressed in requirements
5. Write the synthesis: `memory_write(type="research", tags=["research", "synthesis"])`
6. Include wikilinks to all four dimension notes and the brief in the Relations section

See `cookbook/planning-templates.md` for the synthesis template.

### Step 6: Requirements Definition

Generate categorized requirements with REQ-ID identifiers.

1. Draw requirements from the brief, research findings, and synthesis recommendations
2. Assign REQ-IDs using the format `CATEGORY-NN` (e.g., SETUP-01, PLAN-03, AUTH-02)
3. Group requirements by domain category, not by milestone or timeline
4. Classify each requirement as v1, v2, or out-of-scope
5. Include a traceability table mapping requirements to research findings and roadmap phases
6. Write to memory: `memory_write(type="requirement", tags=["planning", "requirements"])`
7. Include wikilinks to the brief, research synthesis, and roadmap in the Relations section

See `cookbook/planning-templates.md` for the requirements template.

### Step 7: Roadmap Generation

Create a narrative roadmap with phased delivery milestones.

1. Structure the roadmap into phases, each with:
   - **Goal**: What this phase achieves
   - **Depends on**: Which prior phases must complete first
   - **Requirements**: REQ-IDs addressed in this phase
   - **Success criteria**: Testable statements that prove the phase is done
2. Ensure the dependency chain is strictly linear -- each phase builds on the previous
3. Write to memory: `memory_write(type="roadmap", tags=["planning", "roadmap"])`
4. Include wikilinks to requirements, brief, and research notes in the Relations section

The roadmap is a singleton -- only one exists per project. It is immutable after creation per ADR-002. Progress is tracked on the task board, not by editing the roadmap.

See `cookbook/planning-templates.md` for the roadmap template.

### Step 8: Task Board Setup

Create domain-structured epics and features on the task board.

**Epic creation (per ADR-001):**
- Name epics after domain concepts, NOT milestones: "User Authentication System" not "Phase 1"
- Epics may span multiple milestones -- a domain area persists as long as it has active work
- Use `task_create(issue_type="epic", ...)` for each domain area

**Feature creation:**
- Create features under the appropriate epic: `task_create(issue_type="feature", parent="{epic_id}", ...)`
- Include `design` and `acceptance_criteria` fields on features
- Add `memory_refs` linking features to the requirements and roadmap notes

**Sequencing:**
- Set blocker dependencies between epics for milestone sequencing: `task_blockers_add()`
- Only block on real technical/logical dependencies -- do not create artificial sequencing

**Traceability:**
- Add `memory_refs` to each epic and feature linking back to roadmap and requirements notes
- Use `task_update(memory_refs_add=...)` if refs need to be added after creation

See `cookbook/task-templates.md` for hierarchy creation patterns and wave ordering examples.

### Step 9: Verification

Verify all artifacts were created correctly before completing the workflow.

1. **Memory check**: Run `memory_catalog()` and verify these notes exist:
   - Brief (type=brief) -- 1 note
   - Research notes (type=research) -- at least 4 dimension notes + 1 synthesis
   - Requirements (type=requirement) -- at least 1 note
   - Roadmap (type=roadmap) -- 1 note
2. **Task board check**: Run `task_list(issue_type="epic")` and verify domain-structured epics exist
3. **Wikilink check**: Spot-check that Relations sections contain valid wikilinks to existing notes
4. **Traceability check**: Verify that epics and features have `memory_refs` set
5. Report any gaps to the user and offer to fix them

## Output Summary

After a successful run, the following artifacts exist:

**In Djinn memory:**
- 1 project brief (type=brief) -- vision, problem, users, constraints
- 4+ research notes (type=research) -- stack, features, architecture, pitfalls
- 1 research synthesis (type=research, tag=synthesis) -- cross-cutting findings
- 1 requirements note (type=requirement) -- categorized REQ-IDs
- 1 roadmap note (type=roadmap) -- phased milestones with success criteria

**On the task board:**
- Domain-structured epics with sequencing dependencies
- Features under each epic with design and acceptance criteria
- memory_refs linking task board items to memory notes

**In the knowledge graph:**
- Wikilinks connecting all memory notes bidirectionally
- Traceability from requirements through roadmap to task board items

---

## Reference: Phase 2 Extension Points

Phase 2 will implement the full methodology for these areas. The markers below identify where implementation logic will be added:

| Step | Extension Point | What Phase 2 Adds |
|------|----------------|-------------------|
| Step 2 | `[Phase 2 implements the full questioning methodology here]` | Adaptive questioning engine with thread-following, domain exploration, and completeness detection |
| Step 4 | `[Phase 2 implements the parallel research agent pattern here]` | Multi-agent coordination for parallel research across four dimensions |
