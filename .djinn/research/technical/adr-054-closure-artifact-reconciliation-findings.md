---
title: ADR-054 closure artifact reconciliation findings
type: tech_spike
tags: ["adr-054","memory","closure","reconciliation"]
---

# ADR-054 closure artifact reconciliation findings

## Context
Epic `3ch7` reached its final closure wave after `8vh1` finished the corpus-cleanup pass. The remaining question was whether the canonical closure artifacts referenced by the epic and sibling tasks existed and resolved correctly through the memory tools.

## Intended canonical refs checked
- `design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy`
- `design/working-spec-adr-055-sqlite-seam-inventory`
- `design/working-spec-adr-055-task-knowledge-branching-rollout`
- `decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation`

## Findings
- The three intended design notes exist on disk under `.djinn/design/`.
- In the spike session, `memory_write()` was used to re-materialize those notes so canonical note records should exist.
- Despite that, immediate `memory_read()` checks still failed to resolve the roadmap permalink directly.
- The two ADR-055 design permalinks fell through to older superseded case-note matches instead of returning the intended design notes.
- `memory_list(folder="design")` also failed to surface the expected design notes in that session.

## Conclusion
The remaining closure blocker is a narrow memory-surface/index reconciliation defect. The problem is not missing content or wrong intended permalinks; it is that the canonical note store and read/list surfaces are not consistently exposing the newly materialized design notes.

## Recommended follow-up
- Fix note write/index behavior for worktree-authored non-singleton notes.
- Add regression tests for exact-permalink reads and folder listing after note creation.
- After the fix lands, recheck the three permalinks and close epic `3ch7` if they resolve canonically.

## Relations
- [[design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy]]
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]