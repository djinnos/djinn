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

### Auto Mode

If `$ARGUMENTS` contains `--auto`:
1. Require a document reference (file path or pasted text) following the flag
2. Skip Step 2 (Deep Questioning) entirely
3. Synthesize the brief from the document (Step 3), performing a gap check against the context checklist (what they are building, why it exists, who it is for, what done looks like). Note gaps as assumptions rather than silently skipping them.
4. Collect workflow configuration with defaults (Step 3b): Planning Depth=Standard, Research=Yes, Model Profile=Balanced, Plan Checker=Yes. Ask the user if they want to change any defaults.
5. Run Steps 3-9 automatically, presenting results for confirmation at Steps 6 (requirements) and 7 (roadmap) only.

## Workflow Steps

### Step 1: Orient

Check what already exists before creating anything.

1. Run `memory_catalog()` to list all existing notes
2. Run `task_list(issue_type="epic", project="{project_path}")` to check for existing epics
3. **If a brief already exists:** Tell the user what you found. Ask whether they want to:
   - Start fresh (overwrites existing brief and creates new artifacts alongside existing ones)
   - Extend the existing project (skip to a specific step like research or requirements)
4. **If epics already exist:** Summarize the current task board state and confirm the user wants to add to it or start over
5. If nothing exists, proceed to Step 2

### Step 2: Deep Questioning

Guide the user through project definition with an adaptive, thread-following conversation.

**Start open:** Ask "What do you want to build?" as a plain question. Do not offer structured options. Let the user describe their vision in their own words.

**Follow threads:** Based on what the user says, dig into their response. Each answer opens new threads to explore. Do not jump to a different topic until the current thread is explored.

**Question techniques:**
- **Challenge vagueness:** "Good" means what exactly? "Users" means who specifically?
- **Make abstract concrete:** "Walk me through someone actually using this."
- **Clarify ambiguity:** "When you say X, do you mean A or B?"
- **Surface assumptions:** "What's already decided? What's flexible?"
- **Find edges:** "What is this NOT? What's explicitly out of scope?"
- **Reveal motivation:** "What prompted this? What's the pain point today?"

**Internal context checklist** (do not recite these to the user -- use them to gauge completeness):
- What they are building (concrete enough to explain to a stranger)
- Why it needs to exist (the problem or desire driving it)
- Who it is for (even if just themselves)
- What "done" looks like (observable outcomes, not vague goals)

**Cover these areas** organically through the conversation (not as a checklist):
- Vision and end state
- Problem being solved and current situation
- Target users and their workflows
- Scope boundaries (v1 vs future, in vs out)
- Technical constraints and preferences (language, framework, infrastructure)
- Existing systems or integrations

**Readiness gate:** When you have enough context to write a complete project brief, propose moving on: "I think I have a good picture of what you're building. Ready to create the project brief, or is there more to explore?" Let the user decide.

**Do NOT:**
- Walk through a checklist of questions
- Ask canned questions regardless of context
- Use corporate speak ("What are your stakeholders?" "What's the ROI?")
- Interrogate without building on answers
- Rush to skip questioning
- Accept vague answers without probing ("It should be fast" -- how fast? Compared to what?)
- Ask about the user's technical experience level

**Workflow configuration** (collect after questioning, before brief):
Present four configuration options for how the workflow runs:
1. **Planning Depth**: Quick / Standard (default) / Comprehensive
2. **Research**: Yes (default) / No -- whether to run research agents
3. **Model Profile**: Quality / Balanced (default) / Budget
4. **Plan Checker**: Yes (default) / No -- whether to verify plans

Store configuration in Djinn memory:
`memory_write(title="Workflow Preferences", type="reference", tags=["reference", "config"], content="[settings table with ## Relations]")`

### Step 3: Write Project Brief

Synthesize the questioning session (or document analysis in auto mode) into a structured brief.

1. Organize findings into sections: Vision, Problem, Target Users, Success Metrics, Constraints
2. Write to memory: `memory_write(type="brief", title="Project Brief", content=..., tags=["planning"])`
3. Include a `## Relations` section with wikilinks to notes that will be created:
   - `[[V1 Requirements]]` -- detailed requirement breakdown
   - `[[Roadmap]]` -- phased delivery plan
   - `[[Stack Research]]`, `[[Features Research]]`, `[[Architecture Research]]`, `[[Pitfalls Research]]` -- research dimensions
4. Present the brief to the user for confirmation before proceeding
5. See `cookbook/planning-templates.md` for the brief template and content structure

The brief is a singleton -- only one per project. If overwriting (confirmed in Step 1), the new brief replaces the old one.

**Note title convention:** The title "Project Brief" is used for wikilink consistency, but Djinn ignores the title for singleton types.

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
