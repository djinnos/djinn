---
name: init-project
description: Initialize a new project with Djinn. Discovers existing state, guides deep questioning, spawns research subagents, creates the first brief, requirements, roadmap, and epics.
---

# Init Project Workflow

The heavy-lift discovery engine for new projects. Detects greenfield vs brownfield, runs deep adaptive questioning, spawns research subagents, and produces the foundational artifacts: brief, research notes, requirements, roadmap, and domain-structured epics. This only runs once per project. After init, use `/plan` for ongoing steering and `/breakdown` to create tasks.

## Tools

| Tool | Purpose |
|------|---------|
| memory_write | Create brief, research, requirements, and roadmap notes |
| memory_read | Read existing notes for context |
| memory_search | Find notes by keyword to avoid duplicates |
| memory_catalog | Orient at start -- list all existing knowledge |
| memory_edit | Update existing notes (e.g., append Relations backlinks) |
| epic_create | Create domain-structured epics on the task board |
| epic_list | Check for existing epics |

## Do NOT Use

- **Task tools** (task_create, task_update, task_list, task_blockers_add): Init creates epics only. Tasks are created by `/breakdown`.
- **Execution tools** (execution_*, session_for_task): Owned by the coordinator.
- **Task lifecycle tools** (task_claim, task_transition): For execution, not planning.
- **Sync tools** (task_sync_*): Infrastructure concern.
- **Settings/provider tools** (settings_*, provider_*): Admin functions.

### Auto Mode

If `$ARGUMENTS` contains `--auto`:
1. Require a document reference (file path or pasted text) following the flag
2. Skip Step 2 (Deep Questioning) entirely
3. Synthesize the brief from the document (Step 3), performing a gap check against the context checklist (what they are building, why it exists, who it is for, what done looks like). Note gaps as assumptions rather than silently skipping them.
4. Collect workflow configuration with defaults (Step 3b): Planning Depth=Standard, Research=Yes, Model Profile=Balanced, Plan Checker=Yes. Ask the user if they want to change any defaults.
5. Run Steps 3-9 automatically, presenting results for confirmation at Steps 6 (requirements) and 7 (roadmap) only.

## Workflow Steps

### Step 1: Discover Existing State

Check what already exists to determine if this is greenfield or brownfield.

1. Run `memory_catalog()` to list all existing notes
2. Run `epic_list(project=PROJECT)` to check for existing epics
3. **If a brief already exists:** Tell the user what you found. Ask whether they want to:
   - Start fresh (overwrites existing brief and creates new artifacts)
   - Extend the existing project (skip to a specific step)
4. **If epics already exist:** Summarize the current task board state and confirm intent
5. **Brownfield detection:** Check the project directory for existing code:
   - Look for package.json, go.mod, Cargo.toml, pyproject.toml, etc.
   - If code exists, note this is a brownfield project -- codebase research will be included in Step 4
6. If nothing exists, proceed to Step 2

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
- Accept vague answers without probing
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
   - `[[V1 Requirements]]`, `[[Roadmap]]`, research dimension notes
4. Present the brief to the user for confirmation before proceeding

The brief is a singleton -- only one per project. Writing it again overwrites the previous version. This is intentional: the brief is a living document that evolves through `/plan` sessions.

### Step 4: Research

Research across multiple dimensions to build a knowledge base for planning. Execute each dimension sequentially, writing findings to memory before starting the next.

**If Research is disabled** in workflow configuration (Step 2): skip this step entirely.

**For each dimension below**, use WebSearch and WebFetch to research the topic, then write findings to Djinn memory. Be prescriptive ("Use X because Y"), not exploratory ("Options include X, Y, Z"). Verify versions against official sources.

**If brownfield** (detected in Step 1): Add a codebase research dimension before the others. Use Grep and Glob to analyze the existing codebase -- frameworks, patterns, conventions, architecture. Write findings as a research note.

#### Research Dimensions

1. **Stack** -- "What's the standard stack for building [project domain]?"
2. **Features** -- "What features do [project domain] products have? What's table stakes vs differentiating?"
3. **Architecture** -- "How are [project domain] systems typically structured?"
4. **Pitfalls** -- "What do [project domain] projects commonly get wrong?"

Write each dimension:
```
memory_write(
  title="{Dimension} Research",
  type="research",
  tags=["research", "{dimension}"],
  content="[structured findings with ## Relations]"
)
```

All research notes MUST include a `## Relations` section with wikilinks to `[[Project Brief]]` and the other research notes. See `cookbook/planning-templates.md` for templates.

### Step 5: Research Synthesis

Read all research notes and produce a cross-cutting synthesis.

1. Identify **convergent themes** across dimensions
2. Identify **tensions** and propose resolutions
3. Surface **open questions** for requirements
4. Derive **recommendations** for the roadmap
5. Write: `memory_write(title="Research Summary", type="research", tags=["research", "synthesis"], content=...)`

Present a summary of key findings to the user before proceeding.

### Step 6: Requirements Definition

Generate categorized requirements from the brief, research, and synthesis.

1. Assign **REQ-IDs** using `CATEGORY-NN` format (e.g., AUTH-01, DATA-03)
2. Group by **domain category**, not timeline
3. Classify each: **v1** (must have), **v2** (can wait), **Out of scope**
4. Include a traceability table mapping requirements to research findings
5. Write: `memory_write(title="V1 Requirements", type="requirement", tags=["planning", "requirements"], content=...)`

Present requirements to the user. Let them adjust classification before confirming.

### Step 7: Roadmap Generation

Create a narrative roadmap with phased delivery.

1. Group v1 requirements into logical phases with clear goals
2. For each phase: Goal, Depends on, Requirements (REQ-IDs), Success criteria
3. Write: `memory_write(title="Roadmap", type="roadmap", tags=["planning", "roadmap"], content=...)`

The roadmap is a singleton that evolves -- `/plan` can add phases, update goals, and adjust requirements. Present to the user for confirmation.

### Step 8: Epic Creation

Create domain-structured epics on the task board. This step creates **epics only** -- no tasks or features. Task creation happens when `/breakdown` runs.

1. Identify domain areas from the roadmap's requirement categories
2. Group by domain concept: "Authentication System", "Data Pipeline", etc.
3. Create epics:
   ```
   epic_create(
     project=PROJECT,
     title="{Domain Name}",
     description="...",
     emoji="{relevant_emoji}",
     color="{hex_color}"
   )
   ```
4. Name after domain concepts, NOT milestone labels (per ADR-001)
5. Aim for 3-7 epics for a standard project

### Step 9: Verification

Verify all artifacts were created correctly.

1. **Memory check**: `memory_catalog()` -- verify brief, research notes, synthesis, requirements, roadmap exist
2. **Board check**: `epic_list(project=PROJECT)` -- verify domain-structured epics exist
3. **Wikilink check**: Spot-check that `## Relations` sections contain valid wikilinks
4. Report any gaps and offer to fix them

### Step 10: Handoff

Tell the user what was created and what to do next:

"Project initialized. You have:
- A project brief in memory
- {N} research notes with synthesis
- Categorized requirements with REQ-IDs
- A phased roadmap
- {N} domain-structured epics on the board

**Next steps:**
- Run `/clear` to free context
- Use `/plan` to discuss and refine any phase before breaking it down
- Use `/breakdown` to create tasks for a phase"

## Output Summary

After a successful run:

**In Djinn memory:**
- 1 project brief (type=brief)
- 4+ research notes (type=research) with dimension tags
- 1 research synthesis (type=research, tag=synthesis)
- 1 requirements note (type=requirement)
- 1 roadmap note (type=roadmap)
- 1 workflow preferences note (type=reference)

**On the task board:**
- 3-7 domain-structured epics (named after domain concepts, not milestones)
- No tasks yet -- those come from `/breakdown`

**In the knowledge graph:**
- Wikilinks connecting all memory notes bidirectionally
