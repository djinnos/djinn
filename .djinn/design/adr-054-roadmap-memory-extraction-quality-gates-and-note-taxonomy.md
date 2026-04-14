---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","quality-gates","taxonomy","closure"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
Ready for closure verification completion. The prerequisite memory-surface fixes (`16zt` persistence/index visibility and `9f1v` exact-permalink read/list hardening) have landed, and this final pass confirmed the intended closure artifacts are present canonically on disk and aligned as the only remaining ADR-054 closure targets for epic `3ch7`.

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
- Final memory-surface reconciliation evidence was rerun in `c0dv`.

## Canonical closure refs
The three canonical design refs that gate ADR-054 closure are:
- `design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy`
- `design/working-spec-adr-055-sqlite-seam-inventory`
- `design/working-spec-adr-055-task-knowledge-branching-rollout`

These are the intended planner-facing closure artifacts for epic `3ch7`.

## Closure guidance
ADR-054 should close immediately after planner review confirms those three permalinks resolve canonically through memory surfaces. The wider broken-link/orphan backlog remains classified as post-closure memory-hygiene debt, not ADR-054 incompleteness.

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[design/working-spec-adr-055-sqlite-seam-inventory]]
- [[design/working-spec-adr-055-task-knowledge-branching-rollout]]
- [[research/technical/adr-054-closure-artifact-reconciliation-findings]]
- [[reference/project-memory-broken-link-and-orphan-backlog-triage]]
