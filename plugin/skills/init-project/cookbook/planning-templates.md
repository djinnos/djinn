# Planning Templates Cookbook

Memory output patterns for all planning artifact types. Use these as copy-paste templates when writing planning artifacts to Djinn memory.

## Quick Reference

| Artifact | Djinn Type | Singleton? | Key Sections |
|----------|-----------|------------|--------------|
| Project Brief | `type="brief"` | Yes (one per project, mutable) | Vision, Problem, Target Users, Success Metrics, Constraints, Relations |
| Research Note | `type="research"` | No | Summary, Findings, Recommendations, Relations |
| Requirements | `type="requirement"` | No | Overview, v1 Requirements, v2 Requirements, Out of Scope, Traceability |
| Roadmap | `type="roadmap"` | Yes (one per project, mutable) | Overview, Phases, Progress |
| ADR | `type="adr"` | No | Context, Decision, Consequences, Relations |
| Reference | `type="reference"` | No | Varies by purpose |

**Singletons:** `brief` and `roadmap` types allow only one note per project. Writing again overwrites the previous version. Both are **living documents** that evolve as the project progresses -- `/plan` can update them at any time.

---

## Project Brief (type="brief")

```
memory_write(
  title="Project Brief",
  type="brief",
  content="""
# Project Brief

## Vision
TaskFlow is an AI-native project management system that lets development teams plan, track, and execute work through conversational AI agents.

## Problem
Current project management tools require constant manual curation. Teams spend more time managing work than doing it.

## Target Users
- **Development teams (3-15 people)** who use AI coding assistants
- **Tech leads** who need milestone visibility without micromanaging
- **Solo developers** who want structured planning without overhead

## Success Metrics
- Project setup from zero to actionable task board in under 30 minutes
- Every task on the board traces back to a requirement ID
- Milestone progress is queryable in real-time

## Constraints
- Must work within existing MCP tool ecosystem
- Planning workflows must produce Djinn-native artifacts
- All progress tracking derived from live task board state

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
- Brief is a living document -- `/plan` can rewrite it as the project evolves
- The `## Relations` section creates wikilinks in the knowledge graph

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
[Key findings in 2-3 sentences]

## Findings
[Detailed findings organized by sub-topic with tables and comparisons]

## Recommendations
[Prescriptive recommendations: "Use X because Y"]

## Relations
- [[Project Brief]] -- project context
- [[Architecture Research]] -- related dimension
- [[V1 Requirements]] -- requirements that constrain choices
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
[Cross-cutting synthesis in 2-3 sentences]

## Cross-Cutting Findings
### Convergent Themes
[Findings that appear across dimensions]

### Tensions Resolved
[Conflicts between dimensions and their resolutions]

### Open Questions for Requirements
[Unresolved questions]

## Recommendations
[Actionable recommendations for the roadmap]

## Relations
- [[Project Brief]]
- [[Stack Research]]
- [[Features Research]]
- [[Architecture Research]]
- [[Pitfalls Research]]
""",
  tags=["research", "synthesis"]
)
```

**Dimension tags:** `stack`, `features`, `architecture`, `pitfalls`, `synthesis`

---

## Requirements (type="requirement")

```
memory_write(
  title="V1 Requirements",
  type="requirement",
  content="""
# V1 Requirements

## Overview
[Central value proposition in one sentence]

## v1 Requirements

### {Category}
- **{CAT}-01**: [Requirement description]
- **{CAT}-02**: [Requirement description]

### {Category}
- **{CAT}-01**: [Requirement description]

## v2 Requirements
- **{CAT}-01**: [Deferred requirement]

## Out of Scope
- [Explicitly excluded item]

## Traceability
| Requirement | Roadmap Phase | Epic | Status |
|-------------|---------------|------|--------|
| {CAT}-01 | Phase 1 | {Epic} | Planned |

## Relations
- [[Project Brief]] -- project vision
- [[Research Summary]] -- research findings
- [[Roadmap]] -- phased delivery plan
""",
  tags=["planning", "requirements"]
)
```

**REQ-ID format:** `CATEGORY-NN` (e.g., AUTH-01, DATA-03). Group by domain, not timeline.

---

## Roadmap (type="roadmap")

```
memory_write(
  title="Roadmap",
  type="roadmap",
  content="""
# Roadmap

## Overview
[Delivery strategy in 2-3 sentences]

## Phases

### Phase 1: {Title}
**Goal**: [Outcome statement]
**Depends on**: Nothing (first phase)
**Requirements**: {REQ-IDs}
**Success Criteria**:
  1. [Testable statement]
  2. [Testable statement]

### Phase 2: {Title}
**Goal**: [Outcome statement]
**Depends on**: Phase 1
**Requirements**: {REQ-IDs}
**Success Criteria**:
  1. [Testable statement]
""",
  tags=["planning", "roadmap"]
)
```

**Key points:**
- Roadmap is a living document -- `/plan` can add phases, update goals, adjust requirements
- Each phase has: Goal, Depends on, Requirements (REQ-IDs), Success Criteria
- Success criteria are testable statements, not vague descriptions

---

## ADR (type="adr")

```
memory_write(
  title="ADR-001: Hierarchy Mapping",
  type="adr",
  content="""
# ADR-001: Hierarchy Mapping

## Context
[What prompted this decision]

## Decision
[The choice made and why]

## Consequences
**Positive:**
- [Benefit]

**Negative:**
- [Trade-off]

## Relations
- [[Roadmap]]
- [[V1 Requirements]]
""",
  tags=["adr", "architecture"]
)
```

**Title format:** `ADR-NNN: Description`. Tags: `"adr"` plus domain area.

---

## Reference (type="reference")

```
memory_write(
  title="{Phase} Scope",
  type="reference",
  content="""
# {Phase} Scope

## In Scope
- [Specific deliverable]

## Out of Scope
- [Deferred item] -- Reason: [why]

## Preferences
- [Implementation style choice]

## Relations
- [[Roadmap]]
- [[ADR-001: {title}]]
""",
  tags=["scope", "reference"]
)
```

---

## Wikilink Patterns

Every planning artifact should include a `## Relations` section with wikilinks.

### Rules
- Use the note's **title** inside double brackets: `[[Note Title]]`
- Add annotation after `--`: `[[V1 Requirements]] -- detailed breakdown`
- Link bidirectionally: if A links to B, B should link back to A
- Use `memory_edit()` to append backlinks to existing notes

### Building a Connected Graph
1. Write new note with `## Relations` referencing existing notes
2. For each referenced note, append a backlink via `memory_edit()`

---

## Common Mistakes

1. **Forgetting wikilinks in Relations.** Every note should link to at least one other note.

2. **Using brief or roadmap type for non-singleton content.** Writing a second brief overwrites the first. Use `type="research"` or `type="reference"` for non-singleton content.

3. **Not running `memory_catalog()` before writing.** Check what exists first to avoid duplicates.

4. **Putting implementation details in research notes.** Specific decisions belong in ADRs. Implementation designs belong in task `design` fields.
