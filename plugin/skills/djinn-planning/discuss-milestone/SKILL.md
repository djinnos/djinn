# Discuss Milestone Workflow

Facilitate an adaptive discussion about a specific milestone before planning begins. This workflow captures design decisions as ADRs and scope boundaries in memory, enriching context for plan-milestone. It is a READ-heavy, WRITE-light workflow -- it reads extensively from the knowledge base and writes only ADR notes and scope reference notes as discussion outcomes.

## Arguments

`$ARGUMENTS` contains the milestone number (e.g., "2" for Phase 2). Parse the number from the router's forwarded arguments. If no number is provided, ask the user which milestone they want to discuss.

## Tools

| Tool | Purpose |
|------|---------|
| memory_write | Create ADR notes and scope boundary notes |
| memory_read | Read roadmap, requirements, existing ADRs |
| memory_search | Find relevant context across knowledge base |
| memory_catalog | Orient at session start |
| memory_edit | Update existing notes with discussion outcomes |

## Do NOT Use

These tools are outside this workflow's scope:

- **Task tools** (task_create, task_update, task_list, task_count, task_ready, task_children_list, task_blockers_add, task_claim, task_transition): Discussion does not modify the task board -- that is plan-milestone's job
- **Execution tools** (execution_start, execution_pause, execution_status, execution_phase_list, session_for_task): Owned by the Djinn coordinator, not planning workflows
- **Sync tools** (task_sync_push, task_sync_pull, task_sync_status): Infrastructure concern, not workflow
- **Settings tools** (settings_get, settings_update): Admin functions, not workflow
- **Memory destruction tools** (memory_delete, memory_move): Discussion captures context, it does not reorganize the knowledge base

## Workflow Steps

### Step 1: Load Milestone Context

Orient yourself in the knowledge base, then load everything relevant to the target milestone.

1. Run `memory_catalog()` to see all notes in the knowledge base
2. Read the roadmap: `memory_read("roadmap")` (or search for it)
3. Find the target milestone/phase by the number from `$ARGUMENTS`
4. Read its requirements and success criteria from memory
5. Search for existing research notes related to this milestone: `memory_search("milestone {N}")` or by relevant keywords
6. Search for existing ADRs that might be relevant: `memory_search("adr")`
7. Read the artifact mapping reference if it exists: `memory_read("reference/artifact-mapping")`

After loading, you should have a clear picture of:
- What the milestone aims to achieve (from roadmap)
- What requirements it must satisfy
- What research already exists
- What architectural decisions are already made

### Step 2: Identify Discussion Topics

Extract gray areas from the milestone's description and requirements. Look for:

- **Ambiguous requirements** -- requirements with multiple valid interpretations where the user's intent is unclear
- **Scope boundaries** -- what is in vs. out for this milestone, especially where adjacent milestones could overlap
- **Technical choices** -- library selection, architecture patterns, API design decisions that have trade-offs
- **Design decisions** -- UX flows, data models, API contracts, naming conventions where there is no single obvious answer
- **Dependency assumptions** -- what this milestone assumes was already built by prior milestones

Present the identified topics to the user as a numbered list. Ask which topic they want to explore first, or suggest starting with the one that has the most downstream impact.

### Step 3: Adaptive Discussion

For each topic, engage in a structured but flexible discussion:

1. **Present what is known** -- share relevant context from research, requirements, and existing ADRs so the user does not have to repeat themselves
2. **Ask focused questions** -- not generic "what do you think?" but specific questions about preferences, constraints, and trade-offs
3. **Follow threads** -- if an answer reveals complexity or a connected concern, explore it before moving on. Do not rigidly stick to the topic list
4. **Capture the decision** -- when the user reaches a conclusion, confirm it explicitly before writing

**Methodology note:** This is NOT a checklist. Adapt based on what the user reveals. Some topics may need 30 seconds ("yes, use that library"), others may need 10 minutes of back-and-forth to work through trade-offs. Let the conversation breathe.

`[Phase 4 implements the full adaptive discussion methodology here, including question generation strategies, thread-following heuristics, and decision confirmation patterns]`

### Step 4: Capture Design Decisions

For each decision made during discussion, write an ADR to Djinn memory:

```
memory_write(
  type="adr",
  title="ADR-NNN: {decision title}",
  content="""
  ## Context
  [What prompted this decision -- the ambiguity or trade-off]

  ## Decision
  [The choice made and why]

  ## Consequences
  [What follows from this decision -- both positive and negative]

  ## Relations
  - [[Roadmap]] -- Milestone {N}
  - [[{related requirement title}]]
  - [[{related ADR if any}]]
  """,
  tags=["adr", "milestone-{N}"]
)
```

See [cookbook/planning-templates.md](../cookbook/planning-templates.md) for the full ADR template with all fields and wikilink conventions.

**ADR numbering:** Check existing ADRs via `memory_search("adr")` and continue the sequence. If ADR-001 and ADR-002 exist, the next is ADR-003.

`[Phase 4 implements ADR quality checks and cross-referencing logic here]`

### Step 5: Capture Scope Boundaries

After all topics are discussed, write a scope reference note that plan-milestone will consume:

```
memory_write(
  type="reference",
  title="Milestone {N} Scope",
  content="""
  # Milestone {N} Scope

  ## In Scope
  - [Specific deliverable 1 -- with enough detail to be unambiguous]
  - [Specific deliverable 2]

  ## Out of Scope
  - [Thing 1] -- Reason: [why it is deferred or excluded]
  - [Thing 2] -- Reason: [belongs to milestone {M} instead]

  ## Preferences
  - [Implementation style choice 1 -- e.g., "prefer X library over Y"]
  - [Implementation style choice 2 -- e.g., "keep API surface minimal"]

  ## Relations
  - [[Roadmap]] -- Milestone {N}
  - [[ADR-003: {relevant decision}]]
  - [[ADR-004: {relevant decision}]]
  """,
  tags=["scope", "milestone-{N}", "reference"]
)
```

This note is the primary input for plan-milestone when it runs. It provides the decisions and boundaries that constrain task decomposition.

`[Phase 4 implements scope note validation and completeness checks here]`

### Step 6: Summary

Present a summary of the discussion session to the user:

1. **Decisions made** -- list each ADR created with its title and permalink
2. **Scope boundaries set** -- summarize what is in/out for this milestone
3. **Preferences captured** -- list any implementation style choices
4. **Open items** -- if any topics were deferred or need further research, note them

Confirm with the user that the captured context is accurate. If they want to revise anything, use `memory_edit` to update the relevant note.

## Output Summary

After running this workflow, the following artifacts exist in Djinn memory:

- **ADR notes** (type=adr) -- one per design decision, wikilinked to roadmap and requirements
- **Scope reference note** (type=reference) -- "Milestone {N} Scope" with in/out/preferences, consumed by plan-milestone
- **Wikilinks** connecting decisions to the roadmap, requirements, and each other

This workflow does NOT create tasks, modify the task board, or change execution state. It only enriches the knowledge base so that plan-milestone has richer context to work with.
