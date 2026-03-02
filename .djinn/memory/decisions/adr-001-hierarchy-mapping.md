---
title: "ADR-001: Hierarchy Mapping"
type: adr
tags:
  - architecture
  - hierarchy
  - phase-0
---
# ADR-001: Hierarchy Mapping

## Status

Accepted

## Context

The Djinn planning system adapts GSD's methodology for Djinn's MCP-based
memory and task systems. GSD has a 3-level hierarchy (milestone > phase > plan)
while Djinn has a strict hierarchy (epic > feature > task/bug). PROJECT.md
contained contradictory mappings: the Planning->Djinn table implied
domain-structured epics, while a Key Decisions entry said "Phases -> Epics
not Features." This ambiguity would infect every workflow if left unresolved.

## Decision

Roadmap milestones are **narrative only**. They exist as content in the
roadmap memory note (type=roadmap), describing goals, success criteria,
and which epics they require. They are NOT task board entities.

The task board uses **domain-structured epics**:

- **Epic**: Strategic domain (e.g., "Auth System", "Payment Engine") -- NOT "Milestone 1"
- **Feature**: Deliverable (2-4h agent session) under an epic
- **Task**: Implementation step (one commit, one outcome) under a feature
- **Bug**: Defect under a feature

Milestone sequencing is expressed via **blocker dependencies** on the
underlying features/tasks, based on real technical needs. If M2 features
don't depend on M1 work, they can run in parallel.

### Disambiguation Table

| Term | Meaning | Lives In |
|------|---------|----------|
| Roadmap milestone | Narrative goal with success criteria | Roadmap memory note (type=roadmap) |
| Djinn execution phase | Auto-generated grouping of ready tasks | Execution coordinator (ephemeral) |
| Epic | Strategic domain grouping | Task board (persistent) |
| Feature | Deliverable unit (2-4h) | Task board under an epic |
| Task | Implementation step (one commit) | Task board under a feature |

A roadmap milestone is a narrative goal in the roadmap memory note. A Djinn
execution phase is an auto-generated grouping of ready tasks by the execution
coordinator. **They are independent concepts.**

## Consequences

- **Good**: Single consistent hierarchy across all workflows
- **Good**: Domain-structured epics give meaningful kanban board groupings
- **Good**: Parallel execution possible when milestones are independent
- **Bad**: No 1:1 mapping from GSD "phase" to a single Djinn entity type
- **Mitigation**: The [[Artifact Mapping]] reference note provides the
  complete mapping table for workflow authors

## Relations

- [[ADR-002: State Derivation]]
- [[Artifact Mapping]]
- [[Roadmap]]
