---
title: "Artifact Mapping"
type: reference
tags:
  - reference
  - mapping
  - phase-0
---
# Artifact Mapping: GSD -> Djinn

Complete mapping of every GSD planning artifact to its Djinn representation.
Workflows discover this mapping at runtime via:
`memory_read(identifier='reference/artifact-mapping')`

## Memory Artifacts

| GSD Artifact | Djinn Type | MCP Tool Call | Status |
|---|---|---|---|
| PROJECT.md (brief) | `brief` | `memory_write(type="brief", title="Project Brief", ...)` | v1 |
| REQUIREMENTS.md | `requirement` | `memory_write(type="requirement", title="V1 Requirements", ...)` | v1 |
| ROADMAP.md | `roadmap` | `memory_write(type="roadmap", title="Roadmap", ...)` | v1 |
| research/STACK.md | `research` | `memory_write(type="research", title="Stack Research", tags=["stack"])` | v1 |
| research/FEATURES.md | `research` | `memory_write(type="research", title="Features Research", tags=["features"])` | v1 |
| research/ARCHITECTURE.md | `research` | `memory_write(type="research", title="Architecture Research", tags=["architecture"])` | v1 |
| research/PITFALLS.md | `research` | `memory_write(type="research", title="Pitfalls Research", tags=["pitfalls"])` | v1 |
| research/SUMMARY.md | `research` | `memory_write(type="research", title="Research Summary", tags=["synthesis"])` | v1 |
| CONTEXT.md (milestone discussion) | `research` | `memory_write(type="research", title="Milestone N Context", ...)` | v1 |
| Phase ADRs | `adr` | `memory_write(type="adr", title="ADR-NNN: Title", ...)` | v1 |
| config.json (preferences) | `reference` | `memory_write(type="reference", title="Workflow Config", ...)` | v1 |
| STATE.md | N/A | Derived from live queries (see [[ADR-002: State Derivation]]) | N/A |
| Plan files (PLAN.md) | N/A | Tasks created directly via `task_create` | N/A |
| .planning/ directory | N/A | Replaced entirely by Djinn memory + task board | N/A |

## Task Board Artifacts

| GSD Concept | Djinn Entity | MCP Tool Call | Status |
|---|---|---|---|
| Roadmap milestone | Narrative in roadmap note | `memory_read(identifier="roadmap")` | v1 |
| Epic (domain) | `epic` | `task_create(issue_type="epic", title="Auth System", ...)` | v1 |
| Feature (deliverable) | `feature` | `task_create(issue_type="feature", parent=epic_id, ...)` | v1 |
| Task (implementation) | `task` | `task_create(issue_type="task", parent=feature_id, ...)` | v1 |
| Bug (defect) | `bug` | `task_create(issue_type="bug", parent=feature_id, ...)` | v1 |
| Wave ordering | Blocker dep | `task_blockers_add(id=wave2_task, blocking_id=wave1_task)` | v1 |
| Milestone sequencing | Blocker dep | `task_blockers_add(id=m2_feature, blocking_id=m1_feature)` | v1 |
| Progress check | Live query | `task_count(project=P, group_by="status")` | v1 |
| Milestone completion | Live query | `task_list(parent=epic_id, status="closed")` vs total | deferred |
| Phase execution | Djinn execution | `execution_start(project=P)` | N/A (djinn-owned) |

## Relations

- [[ADR-001: Hierarchy Mapping]]
- [[ADR-002: State Derivation]]
- [[Roadmap]]
