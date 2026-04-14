---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","quality-gates","taxonomy"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
In progress — implementation and cleanup waves are landed, but epic closure is now blocked on a narrow memory-surface reconciliation defect rather than missing ADR-054 content.

## Goal
Tighten extraction quality in `llm_extraction.rs` so durable memory writes are gated by stronger note taxonomy, structured templates, semantic novelty checks, and real access signals instead of permissive session-extraction defaults.

## Landed work
- Extraction quality-gate decisions implemented in `llm_extraction.rs`.
- Structured templates enforced for durable `pattern` / `pitfall` / `case` notes.
- Working Spec routing added for non-durable extracted knowledge.
- MCP memory search/retrieval access tracking extended so freshness signals are real.
- Corpus audit tooling landed for ADR-054 cleanup classification.
- Corpus cleanup pass landed and rerun evidence was captured in `8vh1`.
- Narrow roadmap/design canonical-link cleanup landed for current planning artifacts.
- Residual broken-link/orphan backlog was classified narrowly so ADR-054 closure does not expand into historical alias cleanup.

## Closure blocker discovered in `lnvm`
The intended canonical closure artifacts now exist on disk, but the memory surface in this session still fails to resolve them canonically:
- `design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy`
- `design/working-spec-adr-055-sqlite-seam-inventory`
- `design/working-spec-adr-055-task-knowledge-branching-rollout`

Observed failure mode from spike `lnvm`:
- `memory_read()` does not resolve the roadmap permalink directly.
- The two ADR-055 Working Spec permalinks fall through to superseded case-note matches instead of exact design-note resolution.
- `memory_list(folder="design")` also failed to surface the expected design notes in this session.

This points to a memory-surface/index reconciliation problem, not missing note content.

## Next wave
1. Fix note-write/index behavior so worktree-authored non-singleton notes become canonical database records immediately and resolve by exact permalink.
2. Add regression coverage for `memory_read`, `memory_list`, and fallback-search behavior so exact permalink reads cannot be hijacked by older case-note content when the canonical design note exists.
3. Reconcile the three ADR-054 closure refs after the fix lands so the epic can close without dangling or misleading memory refs.

## Closure guidance
ADR-054 should close immediately after the new reconciliation wave proves those three permalinks resolve canonically through memory tools. The wider broken-link/orphan backlog remains classified as post-closure memory-hygiene debt, not ADR-054 incompleteness.

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[research/technical/adr-054-closure-artifact-reconciliation-findings]]
- [[reference/project-memory-broken-link-and-orphan-backlog-triage]]