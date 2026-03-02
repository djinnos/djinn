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

Research across four dimensions to build a knowledge base for planning. Execute each dimension sequentially, writing findings to memory before starting the next.

**If Research is disabled** in workflow configuration (Step 2): skip this step entirely and proceed to Step 5 (synthesize from questioning session only).

**For each dimension below**, use WebSearch and WebFetch to research the topic, then write findings to Djinn memory. Be prescriptive ("Use X because Y"), not exploratory ("Options include X, Y, Z"). Verify versions against official sources. Flag findings with LOW confidence that need validation.

#### Research: Stack

Investigate the standard technology stack for building the user's project.

**Focus areas:**
- Core frameworks and runtime (with current versions)
- Database and persistence layer
- Key supporting libraries and tools
- Deployment and infrastructure patterns

**Core question:** "What's the standard stack for building [project domain] in 2026?"

Write findings:
```
memory_write(
  title="Stack Research",
  type="research",
  tags=["research", "stack"],
  content="[structured findings -- see cookbook/planning-templates.md]

  ## Relations
  - [[Project Brief]] -- project context
  - [[Features Research]] -- feature needs inform stack choice
  - [[Architecture Research]] -- architecture patterns drive stack decisions
  - [[Pitfalls Research]] -- risks to consider in stack selection"
)
```

#### Research: Features

Analyze what features products in this domain typically have.

**Focus areas:**
- Table stakes features (must-have for viability)
- Differentiating features (competitive advantages)
- Common UX patterns and user expectations
- Feature prioritization signals from the market

**Core question:** "What features do [project domain] products have? What's table stakes vs differentiating?"

Write findings: `memory_write(title="Features Research", type="research", tags=["research", "features"], content="[findings with ## Relations]")`

#### Research: Architecture

Investigate how systems in this domain are typically structured.

**Focus areas:**
- Major system components and their responsibilities
- Data models and relationships
- Integration patterns with external services
- Scalability and performance considerations

**Core question:** "How are [project domain] systems typically structured? What are the major components?"

Write findings: `memory_write(title="Architecture Research", type="research", tags=["research", "architecture"], content="[findings with ## Relations]")`

#### Research: Pitfalls

Identify common mistakes and risks in this domain.

**Focus areas:**
- Critical mistakes that cause project failure
- Anti-patterns teams commonly fall into
- Security and performance pitfalls
- Scope creep risk areas

**Core question:** "What do [project domain] projects commonly get wrong? What are the critical mistakes?"

Write findings: `memory_write(title="Pitfalls Research", type="research", tags=["research", "pitfalls"], content="[findings with ## Relations]")`

---

All research notes MUST include a `## Relations` section with wikilinks to `[[Project Brief]]` and the other three research dimension notes. See `cookbook/planning-templates.md` for the full research note template.

### Step 5: Research Synthesis

Read all research notes and produce a cross-cutting synthesis.

1. Read all four dimension notes: `memory_read(identifier="research/stack-research")`, etc., or use `memory_search(query="research", type="research")` to find them
2. Identify **convergent themes** -- findings that appear across multiple dimensions
3. Identify **tensions** between dimensions and propose resolutions (e.g., stack simplicity vs. feature complexity)
4. Surface **open questions** that need to be addressed during requirements definition
5. Derive **recommendations** that should inform the roadmap
6. Write the synthesis: `memory_write(title="Research Summary", type="research", tags=["research", "synthesis"], content=...)`
7. Include `## Relations` with wikilinks to all four dimension notes and the brief

See `cookbook/planning-templates.md` for the synthesis template.

Present a summary of key findings to the user before proceeding to requirements.

### Step 6: Requirements Definition

Generate categorized requirements from the brief, research, and synthesis.

1. Draw requirements from:
   - The brief (user's stated needs and constraints)
   - Research findings (standard features, architecture patterns)
   - Synthesis recommendations (cross-cutting insights)
2. Assign **REQ-IDs** using the format `CATEGORY-NN` (e.g., AUTH-01, DATA-03, UI-05)
   - Category prefixes should map to domain areas (these will inform epic naming in Step 8)
3. Group requirements by **domain category**, not by timeline or milestone
4. **Classify each requirement:**
   - **v1**: Must have for initial release
   - **v2**: Important but can wait for a second iteration
   - **Out of scope**: Explicitly excluded (prevents scope creep)
5. Include a **traceability table** mapping requirements to research findings that support them
6. Write to memory: `memory_write(title="V1 Requirements", type="requirement", tags=["planning", "requirements"], content=...)`
7. Include `## Relations` with wikilinks: `[[Project Brief]]`, `[[Research Summary]]`, `[[Roadmap]]`

Present requirements to the user organized by category. Let the user adjust classification (move items between v1/v2/out-of-scope) before confirming.

See `cookbook/planning-templates.md` for the requirements template.

### Step 7: Roadmap Generation

Create a narrative roadmap with phased delivery milestones.

1. Identify **phases** by grouping v1 requirements into a logical delivery sequence:
   - Each phase should have a clear goal (outcome, not task)
   - Earlier phases should create foundations that later phases build on
   - A phase should be completable in a focused effort (not months of work)
2. For each phase, define:
   - **Goal**: What this phase achieves (outcome-shaped)
   - **Depends on**: Which prior phases must complete first
   - **Requirements**: REQ-IDs addressed in this phase (every v1 requirement must appear in exactly one phase)
   - **Success criteria**: 3-5 testable statements that prove the phase is done
3. Ensure the dependency chain is well-ordered -- each phase builds on the previous where there are real dependencies. Independent phases CAN run in parallel.
4. Write to memory: `memory_write(title="Roadmap", type="roadmap", tags=["planning", "roadmap"], content=...)`
5. Include `## Relations` with wikilinks: `[[Project Brief]]`, `[[V1 Requirements]]`, `[[Research Summary]]`

The roadmap is a singleton and is immutable after creation per ADR-002. Progress is tracked on the task board, not by editing the roadmap.

Present the roadmap to the user for confirmation. This is the last approval gate before task board creation.

See `cookbook/planning-templates.md` for the roadmap template.

**Workflow configuration storage:** After the roadmap is confirmed, store the configuration preferences collected in Step 2:
`memory_write(title="Workflow Preferences", type="reference", tags=["reference", "config"], content=...)`
Include a `## Relations` wikilink to `[[Project Brief]]`. See `cookbook/planning-templates.md` for the reference note pattern.

### Step 8: Task Board Setup

Translate the roadmap into domain-structured epics and features on the Djinn task board. This step has no GSD equivalent -- it bridges the gap between narrative planning (memory) and executable work (task board).

**8a. Identify domain areas** from the roadmap:
- Look at which requirement categories (CATEGORY-NN prefixes) each phase addresses
- Group by domain concept: "Authentication System", "Data Pipeline", "Content Management"
- A domain area may span multiple phases
- Aim for 3-7 epics for a standard project

**8b. Create epics** for each domain area:
```
task_create(
  issue_type="epic",
  title="{Domain Name}",
  project="{project_path}",
  description="...",
  emoji="{relevant_emoji}",
  color="{hex_color}"
)
```
- Name after domain concepts, NOT milestone labels (per ADR-001): "Authentication System" not "Phase 1"
- Include emoji and color for visual identity on the board

**8c. Create features** under each epic for phase-specific work:
```
task_create(
  issue_type="feature",
  parent="{epic_id}",
  project="{project_path}",
  title="{Specific deliverable}",
  description="...",
  design="...",
  acceptance_criteria=[...],
  memory_refs=["requirements/v1-requirements", "roadmap"]
)
```
- Features correspond to deliverables within a roadmap phase for that domain area
- Include `design`, `acceptance_criteria`, and `memory_refs` on every feature

**8d. Set cross-phase blocker dependencies:**
```
task_blockers_add(
  id="{phase_N+1_feature_id}",
  blocking_id="{phase_N_feature_id}",
  project="{project_path}"
)
```
- Features in Phase N+1 must be blocked by **at least one** feature in Phase N that they actually depend on
- Only block on real technical or logical dependencies -- do NOT block every Phase 2 feature on every Phase 1 feature
- Let the Djinn coordinator parallelize everything that is not explicitly blocked

**8e. Add traceability links:**
- Every epic and feature should have `memory_refs` pointing to `["requirements/v1-requirements", "roadmap"]`
- Use `task_update(id=..., memory_refs_add=[...])` if refs need to be added after creation

**Individual tasks are NOT created during new-project.** Only epics and features. Task decomposition into individual work items happens when `plan-milestone` runs for each phase.

See `cookbook/task-templates.md` for epic/feature creation patterns and wave ordering examples.

### Step 9: Verification

Verify all artifacts were created correctly.

1. **Memory check**: Run `memory_catalog()` and verify:
   - 1 brief (type=brief)
   - 4+ research notes (type=research) with dimension tags
   - 1 research synthesis (type=research, tag=synthesis)
   - 1 requirements note (type=requirement)
   - 1 roadmap note (type=roadmap)
   - 1 workflow preferences note (type=reference)
2. **Task board check**: Run `task_list(issue_type="epic", project="{project_path}")` and verify:
   - Domain-structured epics exist (3-7 for a standard project)
   - Each epic has features under it
3. **Wikilink check**: Spot-check that `## Relations` sections contain valid wikilinks by reading 2-3 notes and confirming linked titles exist
4. **Traceability check**: Verify epics and features have `memory_refs` set by inspecting a sample via `task_show(id="{epic_id}")`
5. Report any gaps to the user and offer to fix them

## Output Summary

After a successful run, the following artifacts exist:

**In Djinn memory:**
- 1 project brief (type=brief) -- vision, problem, users, success metrics, constraints
- 4 research notes (type=research) -- stack, features, architecture, pitfalls
- 1 research synthesis (type=research, tag=synthesis) -- cross-cutting findings and recommendations
- 1 requirements note (type=requirement) -- categorized REQ-IDs with v1/v2/out-of-scope classification
- 1 roadmap note (type=roadmap) -- phased milestones with success criteria and requirement traceability
- 1 workflow preferences note (type=reference) -- planning depth, research toggle, model profile, plan-checker toggle

**On the task board:**
- 3-7 domain-structured epics (named after domain concepts, not milestones)
- Features under each epic with design, acceptance criteria, and memory_refs
- Cross-phase blocker dependencies enforcing roadmap sequencing

**In the knowledge graph:**
- Wikilinks connecting all memory notes: brief <-> research <-> synthesis <-> requirements <-> roadmap
- memory_refs linking task board items back to requirements and roadmap notes

---
