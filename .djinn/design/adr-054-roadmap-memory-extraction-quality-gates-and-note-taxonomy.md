---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","closure"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
In progress — implementation work is largely landed, with final closure verification still running.

## Goal
Tighten extraction quality in `llm_extraction.rs` so durable memory writes are gated by stronger note taxonomy, structured templates, semantic novelty checks, and real access signals instead of permissive session-extraction defaults.

## Landed work
- Extraction quality-gate decisions implemented in `llm_extraction.rs`.
- Structured templates enforced for durable `pattern` / `pitfall` / `case` notes.
- Working Spec routing added for non-durable extracted knowledge.
- MCP memory search/retrieval access tracking extended so freshness signals are real.
- Corpus audit tooling landed for ADR-054 cleanup classification.
- Narrow roadmap/design canonical-link cleanup landed for current planning artifacts.

## Remaining closure tasks
- `8vh1` — verify the corpus cleanup pass and record before/after evidence.
- `lnvm` — reconcile final canonical memory refs and closure artifacts.

## Closure guidance
ADR-054 can close after `8vh1` verification is accepted and the final canonical refs used by epic `3ch7` resolve from memory tools. Originated from task `019d89de-7e6b-7651-954f-cc325a0fcf22`, which was dispatched to reconcile ADR-054 closure artifacts and canonical memory refs.

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
<<<<<<< HEAD
<<<<<<< HEAD
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]

## Link cleanup note
- Repaired the stale ADR-053 permalink alias above to the canonical target `[[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]`.
- Residual legacy title-alias debt in adjacent design notes was left to the narrower current-note cleanup pass unless a canonical target was unambiguous.
=======
- [[reference/project-memory-broken-link-and-orphan-backlog-triage]]
- [[cases/classify-residual-broken-wikilinks-by-legacy-alias-type-before-cleanup]]
- [[cases/bucket-intentional-orphan-heavy-folders-separately-in-memory-health-reporting]]
- [[cases/broken-link-backlog-shifted-from-roadmap-artifact-to-legacy-shorthand-adr-title-aliases]]
>>>>>>> origin/main
=======
- [[design/working-spec-adr-055-sqlite-seam-inventory]]
- [[design/working-spec-adr-055-task-knowledge-branching-rollout]]
>>>>>>> origin/main
