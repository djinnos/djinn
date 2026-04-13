---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","extraction","quality-gates"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
Epic remains open. The codebase already appears to satisfy part of ADR-054 Phase 1, but the core extraction-quality work in Phase 2 and the cleanup work in Phase 3 are still unimplemented.

## What is already landed

### 1. Access tracking on memory reads
`server/crates/djinn-mcp/src/tools/memory_tools/ops.rs` already calls `repo.touch_accessed(&note.id)` inside `memory_read`, and `server/crates/djinn-mcp/src/server.rs` records co-access reads through `record_memory_read()`. This means the ADR item to wire access tracking into read paths is at least partially implemented.

### 2. Semantic novelty detection exists
`server/crates/djinn-agent/src/actors/slot/llm_extraction.rs` no longer uses a pure lexical-only dedup decision. It already runs a novelty decision flow and semantic candidate comparison before creating extracted notes, and boosts confidence on an existing note when a semantic duplicate is detected.

## Remaining gap versus ADR-054

The remaining work is the part that actually tightens write quality:

1. **Quality-gate decisions are still too weak**
   - `llm_extraction.rs` currently decides mostly between create-vs-duplicate.
   - ADR-054 requires a richer gate with specificity, generality, durability, novelty, and type-fit checks.
   - Required outcomes are broader than today: `durable_write`, `merge_into_existing`, `downgrade_to_working_spec`, and `discard`.

2. **Durable note templates are not enforced**
   - Extracted `pattern`, `pitfall`, and `case` notes are still accepted without required section structure.
   - ADR-054 requires type-specific section templates and rejection/downgrade when mandatory sections are missing.

3. **Working Spec convention is not represented in extraction flow**
   - The ADR introduces session-scoped Working Specs as a `design`-note convention for mutable task-local understanding.
   - Current extraction still targets only durable `case`/`pattern`/`pitfall` writes.

4. **Phase 3 cleanup is still outstanding**
   - Existing notes were not audited against the stricter templates/taxonomy yet.
   - The epic still needs a focused cleanup wave after the extraction pipeline is upgraded.

## Wave plan

### Wave 1 — Implement extraction quality gate core
- Add explicit extraction-quality scoring / decision model in `llm_extraction.rs`.
- Support the full ADR decision surface, especially `merge_into_existing`, `downgrade_to_working_spec`, and `discard`.
- Preserve existing semantic-duplicate confidence-boost behavior where it still fits.

### Wave 2 — Enforce durable note templates and Working Spec routing
- Update extraction prompting and validation so `pattern`, `pitfall`, and `case` outputs must satisfy required sections.
- Introduce Working Spec routing for notes that are useful but not durable enough for the canonical KB.
- Add tests proving non-conforming notes are downgraded instead of persisted durably.

### Wave 3 — Corpus cleanup and migration pass
- Audit existing extracted `pattern` / `pitfall` / `case` notes.
- Identify notes that should be merged, strengthened, demoted, or archived under the new policy.
- Capture the migration procedure and any follow-on cleanup work.

## This wave
Create focused worker tasks for:
1. quality-gate decision engine in `llm_extraction.rs`
2. template enforcement and prompt/schema updates
3. Working Spec routing and task-scoped storage semantics
4. corpus audit + cleanup tooling/report for pre-existing extracted notes

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[decisions/adr-053-semantic-memory-search-—-candle-embeddings-with-sqlite-vec]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
