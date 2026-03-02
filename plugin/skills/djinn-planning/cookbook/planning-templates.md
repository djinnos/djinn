# Planning Templates Cookbook

Memory output patterns for all planning artifact types. Use these as copy-paste templates when writing planning artifacts to Djinn memory.

## Quick Reference

| Artifact | Djinn Type | Singleton? | Key Sections |
|----------|-----------|------------|--------------|
| Project Brief | `type="brief"` | Yes (one per project) | Vision, Problem, Target Users, Success Metrics, Constraints, Relations |
| Research Note | `type="research"` | No | Summary, Findings, Recommendations, Relations |
| Requirements | `type="requirement"` | No | Overview, v1 Requirements, v2 Requirements, Out of Scope, Traceability |
| Roadmap | `type="roadmap"` | Yes (one per project) | Overview, Phases, Progress |
| ADR | `type="adr"` | No | Context, Decision, Consequences, Relations |
| Reference | `type="reference"` | No | Varies by purpose |

**Singletons:** `brief` and `roadmap` types allow only one note per project. The `title` parameter is ignored for singletons -- Djinn uses a fixed title.

---

## Project Brief (type="brief")

```
memory_write(
  title="Project Brief",
  type="brief",
  content="""
# Project Brief

## Vision
TaskFlow is an AI-native project management system that lets development teams plan, track, and execute work through conversational AI agents rather than manual board manipulation. Teams describe what they want to accomplish; AI handles the decomposition, scheduling, and progress tracking.

## Problem
Current project management tools require constant manual curation -- creating tickets, updating statuses, linking dependencies. Teams spend more time managing work than doing it. Context is scattered across wikis, tickets, and chat threads with no unified knowledge layer.

## Target Users
- **Development teams (3-15 people)** who use AI coding assistants and want AI-driven planning
- **Tech leads** who need milestone visibility without micromanaging individual tasks
- **Solo developers** who want structured planning without the overhead of full PM tools

## Success Metrics
- Project setup from zero to actionable task board in under 30 minutes
- Research phase produces at minimum 4 dimension-specific notes connected via wikilinks
- Every task on the board traces back to a requirement ID
- Milestone progress is queryable in real-time from the task board (no stale dashboards)

## Constraints
- Must work within existing MCP tool ecosystem (no custom tool creation)
- Planning workflows must produce Djinn-native artifacts (memory notes + task board items)
- All progress tracking derived from live task board state, never stored snapshots

## Relations
- [[V1 Requirements]] -- detailed requirement breakdown
- [[Roadmap]] -- phased delivery plan
- [[Stack Research]] -- technology evaluation
""",
  tags=["planning"]
)
```

**Key points:**
- Title parameter is ignored for brief (singleton) but include it for clarity
- The `## Relations` section creates wikilinks to other notes in the knowledge graph
- Tags are optional for brief -- `["planning"]` is sufficient

---

## Research Note (type="research")

### Dimension-Specific Research Note

```
memory_write(
  title="Stack Research",
  type="research",
  content="""
# Stack Research

## Summary
Evaluated four MCP server frameworks for hosting Djinn's planning tools. Bun with Hono provides the best combination of startup speed, TypeScript-native development, and MCP protocol support. SQLite via Drizzle ORM handles the persistence layer with zero-config deployment.

## Findings

### MCP Server Frameworks
| Framework | Startup | TS Support | MCP SDK | Verdict |
|-----------|---------|------------|---------|---------|
| Node + Express | 800ms | Via tsx | Official SDK | Viable but slow startup |
| Bun + Hono | 120ms | Native | Official SDK | Best overall |
| Deno + Oak | 200ms | Native | Community | SDK maturity concern |
| Python FastAPI | 1.2s | N/A | Official SDK | Wrong ecosystem |

### Persistence Options
- **SQLite + Drizzle**: Zero-config, single-file DB, type-safe queries. Ideal for single-machine deployments.
- **PostgreSQL + Prisma**: Production-grade but requires external database. Overkill for planning-only workloads.

### Key Constraints Discovered
- MCP tool responses must be under 100KB to avoid context window pressure on the calling agent
- Memory notes with wikilinks require a catalog index for link resolution
- Task board queries should support filtering by status, parent, and label to enable wave-based execution

## Recommendations
1. Use Bun + Hono for MCP server (startup speed matters for daemon lifecycle)
2. SQLite + Drizzle for persistence (zero-config aligns with plugin distribution model)
3. Implement response pagination for task_list queries exceeding 50 items

## Relations
- [[Project Brief]] -- project context driving these technology choices
- [[Architecture Research]] -- related architectural patterns evaluation
- [[Features Research]] -- feature requirements informing stack selection
- [[V1 Requirements]] -- requirements that constrain technology choices
""",
  tags=["research", "stack"]
)
```

### Research Synthesis Note

```
memory_write(
  title="Research Summary",
  type="research",
  content="""
# Research Summary

## Summary
Synthesis of four research dimensions (stack, features, architecture, pitfalls) for the TaskFlow project. Key findings converge on a Bun + SQLite stack, domain-structured task hierarchy, and conversational planning UX.

## Cross-Cutting Findings

### Convergent Themes
1. **AI-native UX**: All dimensions agree that conversational interfaces outperform form-based input for planning tasks
2. **Live state over snapshots**: Architecture and features research both conclude that progress must be derived from task board state, not stored reports
3. **Domain-structured hierarchy**: Features and architecture converge on epics as domain concepts (not milestones), with blocker dependencies for sequencing

### Tensions Resolved
- **Stack vs. Features**: Stack research favored SQLite simplicity; features research wanted full-text search. Resolution: SQLite FTS5 extension provides both.
- **Architecture vs. Pitfalls**: Architecture proposed parallel agent coordination; pitfalls flagged race conditions. Resolution: Optimistic locking with retry on conflict.

### Open Questions for Requirements
1. Should research notes be editable after synthesis, or treated as immutable snapshots?
2. How many revision iterations should the plan-checker support before escalating to the user?

## Recommendations
1. Proceed with Bun + SQLite + Drizzle stack (unanimous across dimensions)
2. Adopt domain-structured epics with blocker-based wave ordering (architecture + features agreement)
3. Limit plan-checker to 3 revision iterations (pitfalls research recommendation)

## Relations
- [[Project Brief]] -- the project these findings serve
- [[Stack Research]] -- technology evaluation dimension
- [[Features Research]] -- feature analysis dimension
- [[Architecture Research]] -- architecture patterns dimension
- [[Pitfalls Research]] -- risk and anti-pattern dimension
""",
  tags=["research", "synthesis"]
)
```

**Key points:**
- Dimension tags: `stack`, `features`, `architecture`, `pitfalls`, `synthesis`
- Dimension notes focus on one area; synthesis notes cross-reference all dimensions
- Always include `["research", "<dimension>"]` as tags for searchability
- Wikilinks in Relations connect the knowledge graph bidirectionally

---

## Requirements (type="requirement")

```
memory_write(
  title="V1 Requirements",
  type="requirement",
  content="""
# V1 Requirements

## Overview
Core requirements for TaskFlow v1 delivery. The central value proposition is: teams describe intent, AI produces structured plans with full traceability from requirements to tasks.

## v1 Requirements

### Project Setup
- **SETUP-01**: User can initialize a new project through guided questioning (not a static form)
- **SETUP-02**: Questioning adapts based on previous answers, following threads of interest
- **SETUP-03**: Project brief is written to Djinn memory as type=brief singleton

### Research
- **RSRCH-01**: Four parallel research agents produce dimension-specific notes (stack, features, architecture, pitfalls)
- **RSRCH-02**: Each research note is written to Djinn memory as type=research with dimension tag
- **RSRCH-03**: A synthesis agent reads all research and produces a summary note with cross-cutting findings

### Planning
- **PLAN-01**: Roadmap with milestones and success criteria is written to Djinn memory as type=roadmap singleton
- **PLAN-02**: Domain-structured epics are created on the task board (not milestone-named)
- **PLAN-03**: Features and tasks are decomposed with acceptance criteria and design context
- **PLAN-04**: Wave ordering is enforced via blocker dependencies between tasks

### Traceability
- **TRACE-01**: Every task on the board references at least one requirement ID
- **TRACE-02**: Memory notes and tasks are bidirectionally linked via memory_refs and wikilinks

## v2 Requirements

### Progress Tracking
- **PROG-01**: Real-time progress queries derive status from live task board state
- **PROG-02**: Milestone completion percentage calculated from child task statuses

### Discussion
- **DISC-01**: Discuss-milestone captures scope boundaries and design decisions before planning

## Out of Scope
- Manual task board UI (AI-only interaction for v1)
- Multi-team coordination (single-team focus for v1)
- Time estimation and deadline tracking
- Integration with external project management tools (Jira, Linear, etc.)

## Traceability

| Requirement | Roadmap Phase | Epic | Status |
|-------------|---------------|------|--------|
| SETUP-01 | Phase 1: Project Init | Project Setup | Planned |
| SETUP-02 | Phase 1: Project Init | Project Setup | Planned |
| RSRCH-01 | Phase 1: Project Init | Research System | Planned |
| PLAN-01 | Phase 2: Core Planning | Planning Engine | Planned |
| PLAN-02 | Phase 2: Core Planning | Task Decomposition | Planned |
| TRACE-01 | Phase 2: Core Planning | Task Decomposition | Planned |

## Relations
- [[Project Brief]] -- project vision these requirements serve
- [[Research Summary]] -- research findings informing these requirements
- [[Roadmap]] -- phased delivery plan mapping requirements to milestones
""",
  tags=["planning", "requirements"]
)
```

**Key points:**
- REQ-ID format: `CATEGORY-NN` (e.g., SETUP-01, PLAN-03)
- Group requirements by domain category, not by milestone or phase
- v2 requirements are listed separately to show roadmap progression
- Traceability table maps requirements to roadmap phases and task board epics
- Out of Scope section prevents scope creep during planning

---

## Roadmap (type="roadmap")

```
memory_write(
  title="Roadmap",
  type="roadmap",
  content="""
# Roadmap

## Overview
TaskFlow delivers in three phases following a strict dependency chain. Phase 1 establishes the project foundation (brief, research, requirements), Phase 2 builds the core planning engine (task decomposition with wave ordering), and Phase 3 adds depth through discussion workflows and progress tracking. Each phase unlocks capabilities that the next phase depends on.

## Phases

### Phase 1: Project Initialization
**Goal**: A user can go from zero to a fully populated Djinn memory (brief, research, requirements, roadmap) through guided AI interaction.
**Depends on**: Nothing (first phase)
**Requirements**: SETUP-01, SETUP-02, SETUP-03, RSRCH-01, RSRCH-02, RSRCH-03, PLAN-01
**Success Criteria**:
  1. Running new-project triggers adaptive questioning that follows threads
  2. Four parallel research notes are produced with dimension tags and wikilinks
  3. A synthesis note cross-references all research findings
  4. Requirements are generated with REQ-IDs grouped by category
  5. Roadmap note is created with milestone structure and success criteria

### Phase 2: Core Planning
**Goal**: A user can decompose any milestone into a fully structured task board with wave-ordered execution.
**Depends on**: Phase 1 (needs brief, research, requirements, roadmap in memory)
**Requirements**: PLAN-02, PLAN-03, PLAN-04, TRACE-01, TRACE-02
**Success Criteria**:
  1. Domain-structured epics created on task board (not milestone-named)
  2. Features and tasks have acceptance criteria and design context
  3. Wave ordering via blocker dependencies is enforced
  4. Plan-checker validates decomposition against milestone success criteria
  5. Bidirectional linking between tasks and memory notes

### Phase 3: Supporting Workflows
**Goal**: Discussion and progress workflows add depth to the planning loop.
**Depends on**: Phase 2 (needs task board populated)
**Requirements**: PROG-01, PROG-02, DISC-01
**Success Criteria**:
  1. Discuss-milestone captures design decisions as ADR notes
  2. Progress queries derive status from live task board
  3. Milestone completion percentage calculated from child tasks

## Progress

| Phase | Status | Completed |
|-------|--------|-----------|
| 1. Project Initialization | Not started | - |
| 2. Core Planning | Not started | - |
| 3. Supporting Workflows | Not started | - |
""",
  tags=["planning", "roadmap"]
)
```

**Key points:**
- Title parameter is ignored for roadmap (singleton) but include it for clarity
- Each phase has: Goal, Depends on, Requirements (REQ-IDs), Success Criteria
- Success criteria are testable statements, not vague descriptions
- The Progress table tracks delivery status
- Roadmap is immutable after creation per ADR-002 -- use task board for live status

---

## ADR (type="adr")

```
memory_write(
  title="ADR-001 Hierarchy Mapping",
  type="adr",
  content="""
# ADR-001 Hierarchy Mapping

## Context
The planning system needs to map conceptual milestones from the roadmap to executable structures on the task board. Two competing approaches exist: milestones as epics (1:1 mapping) or milestones as a narrative layer with domain-structured epics underneath. The choice affects how agents decompose work and how progress is aggregated.

## Decision
Milestones are narrative-only in the roadmap memory note. The task board uses domain-structured epics (e.g., "User Authentication System" not "Milestone 1"). Sequencing between milestones is achieved through blocker dependencies between tasks, not through epic ordering.

**Key implications:**
- Epics represent domain concepts that may span multiple milestones
- A single milestone's work may touch multiple epics
- "Milestone" and "Djinn execution phase" are independent concepts
- Progress per milestone is derived by querying tasks with milestone-linked requirements

## Consequences
**Positive:**
- Epics remain stable even if milestone boundaries shift
- Work is organized by what it IS (domain) not when it ships (timeline)
- Agents can work on related tasks across milestones without epic confusion

**Negative:**
- Milestone progress requires a query across multiple epics (no single epic = one milestone)
- Requires discipline to name epics by domain, not by timeline

## Relations
- [[ADR-002 State Derivation]] -- related architectural decision on progress tracking
- [[V1 Requirements]] -- PLAN-02 requires domain-structured epics
- [[Roadmap]] -- defines the milestones this ADR addresses
""",
  tags=["adr", "architecture"]
)
```

**Key points:**
- Title format: `ADR-NNN Description` (e.g., "ADR-001 Hierarchy Mapping")
- Four sections: Context, Decision, Consequences, Relations
- Consequences split into Positive and Negative
- Tags include `"adr"` plus the domain area (e.g., `"architecture"`, `"data"`, `"workflow"`)

---

## Reference (type="reference")

```
memory_write(
  title="Artifact Mapping",
  type="reference",
  content="""
# Artifact Mapping

Reference table mapping planning concepts to Djinn storage and MCP tool calls.

## Planning Artifacts to Djinn

| Planning Artifact | Djinn Storage | MCP Tool | Type | Singleton? |
|-------------------|--------------|----------|------|------------|
| Project Brief | Memory note | memory_write | brief | Yes |
| Research Note | Memory note | memory_write | research | No |
| Requirements | Memory note | memory_write | requirement | No |
| Roadmap | Memory note | memory_write | roadmap | Yes |
| ADR | Memory note | memory_write | adr | No |
| Epic | Task board | task_create | epic | No |
| Feature | Task board | task_create | feature | No |
| Task | Task board | task_create | task | No |
| Bug | Task board | task_create | bug | No |
| Blocker Dep | Task board | task_blockers_add | - | No |

## Relations
- [[Project Brief]] -- the brief artifact this mapping covers
- [[V1 Requirements]] -- requirements driving these artifact types
""",
  tags=["reference", "mapping"]
)
```

**Key points:**
- Reference notes are flexible -- use them for configuration, lookup tables, workflow preferences
- No fixed section structure (unlike brief/research/ADR which have standard sections)
- Tags should describe the reference's purpose (e.g., `"mapping"`, `"config"`, `"workflow-prefs"`)

---

## Wikilink Patterns

Every planning artifact should include a `## Relations` section at the end with wikilinks to related notes. This creates a navigable knowledge graph.

### The Relations Section Pattern

```markdown
## Relations
- [[Project Brief]] -- project vision and constraints
- [[V1 Requirements]] -- detailed requirement breakdown
- [[ADR-001 Hierarchy Mapping]] -- hierarchy decision affecting this artifact
- [[Stack Research]] -- technology choices informing implementation
```

### Wikilink Rules
- Use the note's **title** inside double brackets: `[[Note Title]]`
- Add a brief annotation after `--` explaining the relationship
- Link bidirectionally: if Note A links to Note B, Note B should link back to Note A
- Use `memory_edit()` to add Relations entries to existing notes:

```
memory_edit(
  identifier="research/stack-research",
  operation="append",
  content="\n- [[V1 Requirements]] -- requirements informed by this research"
)
```

### Building a Connected Graph
When creating a new note, add wikilinks to existing notes in your Relations section, then update those existing notes to link back:

1. Write the new note with `memory_write()` including `## Relations` referencing existing notes
2. For each referenced note, use `memory_edit()` to append a backlink in that note's Relations section

---

## Common Mistakes

1. **Writing acceptance criteria in memory notes.** Acceptance criteria belong in the `acceptance_criteria` field of `task_create()`, not in memory note content. Memory notes describe WHAT and WHY; task acceptance criteria describe the testable DONE condition.

2. **Forgetting wikilinks in the Relations section.** Every planning artifact should link to at least one other note. Orphaned notes break the knowledge graph and make context assembly harder for agents.

3. **Using brief or roadmap type for non-singleton content.** These types allow only one note per project. Writing a second brief overwrites the first. Use `type="research"` or `type="reference"` for non-singleton content.

4. **Not running memory_catalog() before writing.** Always check what already exists before creating new notes. Duplicate titles create confusion and break wikilink resolution. Start every session with `memory_catalog()` to orient.

5. **Putting implementation details in research notes.** Research notes capture findings and recommendations. Specific implementation decisions belong in `type="adr"` notes. Implementation designs belong in task `design` fields or `type="design"` notes.
