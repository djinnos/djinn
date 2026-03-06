---
name: planning
description: Plan project work through adaptive conversation. Creates new phases, updates the brief and roadmap, captures design decisions as ADRs, and defines scope boundaries. Use before /breakdown.
---

# Plan Workflow

The thinking space for ongoing project steering. Use `/planning` to discuss what needs to happen next -- whether that's a new phase, a new feature area, or refining an existing plan. This workflow reads from the knowledge base, has an adaptive conversation with the user, and writes ADRs, scope notes, and updates to the brief and roadmap. It does NOT create tasks -- that's `/breakdown`'s job.

## Tools

| Tool | Purpose |
|------|---------|
| memory_write | Create ADR notes and scope notes |
| memory_read | Read roadmap, requirements, existing ADRs, brief |
| memory_search | Find relevant context across knowledge base |
| memory_catalog | Orient at session start |
| memory_edit | Update existing notes -- brief, roadmap, scope notes |

## Do NOT Use

- **Task tools** (task_create, task_update, task_list, task_claim, task_transition, epic_tasks): Planning does not modify the task board. That is `/breakdown`'s job.
- **Epic tools** (epic_create, epic_update): Epic creation happens in `/init-project` or `/breakdown`.
- **Execution tools** (execution_*, session_for_task): Owned by the coordinator.
- **Sync tools** (task_sync_*): Infrastructure concern.
- **Settings tools** (settings_*, provider_*): Admin functions.
- **Memory destruction tools** (memory_delete, memory_move): Planning captures context, it does not reorganize the knowledge base.

## Workflow Steps

### Step 1: Orient

Load the current state of the project.

1. Run `memory_catalog()` to see all notes
2. Read the brief: `memory_read("brief")`
3. Read the roadmap: `memory_read("roadmap")`
4. Search for existing ADRs: `memory_search(type="adr")`
5. Check for existing scope notes: `memory_search(type="reference", query="scope")`

After loading, present a brief summary to the user:
- Current roadmap phases and their status
- Number of existing ADRs
- Any scope notes from prior sessions

### Step 2: Determine Intent

Ask the user what they want to plan. Do NOT assume -- ask:

- "What are we planning? A new phase, a new feature area, or refining something existing?"

Based on the answer, the workflow branches:

**New phase:** The user wants to add a phase to the roadmap. Go to Step 3a.
**New feature area:** The user wants to add work within an existing phase. Go to Step 3b.
**Refine existing:** The user wants to discuss and make decisions about an existing phase. Go to Step 3c.

### Step 3a: Plan New Phase

1. Understand what the new phase should achieve -- use adaptive questioning (same techniques as init-project Step 2)
2. Identify which requirements it addresses (existing REQ-IDs or new ones)
3. If new requirements are needed, create them:
   - Read existing requirements: `memory_read("requirement")`
   - Use `memory_edit` to append new REQ-IDs to the requirements note
4. Update the roadmap to add the new phase:
   ```
   memory_edit(
     identifier="roadmap",
     operation="append",
     section="Phases",
     content="### Phase {N}: {Title}\n**Goal**: ...\n**Depends on**: ...\n**Requirements**: ...\n**Success Criteria**: ..."
   )
   ```
5. Update the brief if the project's vision or scope has evolved:
   ```
   memory_write(type="brief", title="Project Brief", content="[updated brief]", tags=["planning"])
   ```
6. Proceed to Step 4 (Discussion) for the new phase

### Step 3b: Plan New Feature Area

1. Understand what the feature area is about
2. Identify which existing phase it belongs to
3. If it introduces new requirements, update the requirements note
4. If it changes the phase's scope, update the roadmap
5. Proceed to Step 4 (Discussion) for the feature area

### Step 3c: Refine Existing

1. Ask which phase or area the user wants to discuss
2. Load all relevant context for that area:
   - Phase details from roadmap
   - Related requirements
   - Existing ADRs and scope notes
3. Proceed to Step 4 (Discussion)

### Step 4: Adaptive Discussion

For the selected topic, engage in structured but flexible discussion.

1. **Present what is known** -- share relevant context from research, requirements, and existing ADRs
2. **Identify gray areas** -- ambiguous requirements, scope boundaries, technical choices, design decisions
3. **Present topics** as a numbered list. Ask which to explore first.

#### Discussion Loop

For each topic:

1. **Announce the topic** and present relevant context. If an existing ADR covers it: "ADR-{NNN} already decided {X}. Want to revisit, or move on?"

2. **Ask focused questions** -- not "What do you think?" but specific trade-off questions:
   - "Should [X] use [A] or [B]? [Brief trade-off]"
   - "What happens when [edge case]?"
   - "Is [constraint] a hard requirement or flexible?"

3. **Follow threads** -- when an answer reveals complexity, explore it before moving on

4. **Decision checkpoint** -- when a decision crystallizes, confirm: "So we're going with [X]. Anything else on this topic?"

5. **Track decisions** internally for ADR writing:
   - What was decided
   - Why (the reasoning)
   - What it affects downstream

6. **Scope creep redirection** -- if the user suggests something outside scope: "That sounds like it belongs elsewhere. I'll note it. For now: [return to current question]"

#### Topic Completion

After all topics are discussed:
- Summarize decisions captured
- If new gray areas emerged, offer to explore them
- When satisfied, announce: "I'll now capture the decisions and scope."

### Step 5: Capture Design Decisions

For each decision made during discussion, write an ADR.

#### Granularity Filter

Before writing, classify each decision:
- **ADR-worthy**: Constrains implementation -- library choices, data models, API contracts, architecture patterns
- **Preference-only**: Style choices, non-binding suggestions -- goes to scope note Preferences section

Test: "Would a different choice here change how tasks are structured or what code gets written?" Yes = ADR. No = preference.

#### ADR Numbering

1. Run `memory_search(query="ADR", type="adr")` to find existing ADRs
2. Parse titles for highest number, continue sequence

#### Write ADRs

```
memory_write(
  type="adr",
  title="ADR-{NNN}: {decision title}",
  content="## Context\n{what prompted this}\n\n## Decision\n{choice and reasoning}\n\n## Consequences\n**Positive:**\n{benefits}\n\n**Negative:**\n{trade-offs}\n\n## Relations\n- [[Roadmap]]\n- [[{related notes}]]",
  tags=["adr", "{domain}"]
)
```

Do NOT ask the user to confirm each ADR. Trust the conversation.

### Step 6: Capture Scope

Write a scope reference note that `/breakdown` will consume:

```
memory_write(
  type="reference",
  title="{Phase/Area} Scope",
  content="# {Phase/Area} Scope\n\n## In Scope\n{deliverables}\n\n## Out of Scope\n{deferred items with reasons}\n\n## Preferences\n{style choices}\n\n## Relations\n- [[Roadmap]]\n- [[ADR-{NNN}: {title}]]",
  tags=["scope", "reference"]
)
```

If a scope note already exists for this area, use `memory_edit` to update it rather than overwriting.

### Step 7: Update Living Documents

If the discussion revealed changes to project direction:

1. **Update the brief** if vision, constraints, or success metrics changed:
   ```
   memory_write(type="brief", title="Project Brief", content="[updated]", tags=["planning"])
   ```

2. **Update the roadmap** if phases, dependencies, or success criteria changed:
   ```
   memory_write(type="roadmap", title="Roadmap", content="[updated]", tags=["planning", "roadmap"])
   ```

3. **Update requirements** if new REQ-IDs were added or classifications changed:
   ```
   memory_edit(identifier="requirements/v1-requirements", ...)
   ```

### Step 8: Summary

Present what was captured:

1. **Decisions made** -- list each ADR with title
2. **Scope boundaries** -- in/out for this area
3. **Preferences** -- implementation style choices
4. **Living document updates** -- what changed in brief/roadmap/requirements
5. **Open items** -- deferred topics or needed research

End with: "Run `/clear` before starting `/breakdown` to create tasks from this plan."

## Output Summary

After running this workflow:

**In Djinn memory:**
- ADR notes (type=adr) -- one per design decision
- Scope reference note (type=reference) -- in/out/preferences for `/breakdown`
- Updated brief and/or roadmap if project direction evolved
- Updated requirements if new REQ-IDs were added

This workflow does NOT create tasks, modify the task board, or change execution state. It enriches the knowledge base so `/breakdown` has clear context.
