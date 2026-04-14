---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","extraction","quality-gates"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
Epic remains open for a final rollout/cleanup wave. Core ADR-054 extraction behavior is now implemented: `llm_extraction.rs` evaluates notes through the richer quality gate, supports `durable_write` / `merge_into_existing` / `downgrade_to_working_spec` / `discard`, enforces durable note templates, and routes non-durable knowledge into task-scoped Working Specs. The repository also now has an extracted-note audit path. The remaining work is to finish access-tracking coverage in MCP memory retrieval flows and to clean up the existing corpus plus roadmap/design hygiene exposed by the audit.

## What is already landed

### 1. Extraction quality-gate decision engine
`server/crates/djinn-agent/src/actors/slot/llm_extraction.rs` now evaluates extracted notes across specificity, generality, durability, novelty, type fit, and required structure, then routes them to `MergeIntoExisting`, `DowngradeToWorkingSpec`, `Discard`, or `DurableWrite`.

### 2. Working Spec routing is implemented
`persist_working_spec()` in `llm_extraction.rs` now creates or updates a task-scoped `design` note named `Working Spec <task-short-id>` so session-local understanding is preserved without polluting the durable pattern/pitfall/case corpus.

### 3. Durable template enforcement is implemented
The extraction flow now rejects or downgrades durable `pattern`, `pitfall`, and `case` notes that do not satisfy ADR-054’s required section structure, rather than accepting generic one-paragraph extracted notes.

### 4. Extracted-note audit tooling exists
`server/crates/djinn-db/src/repositories/note/graph.rs` and the `memory_extracted_audit` MCP surface now classify existing extracted notes into merge, underspecified, demote-to-working-spec, and archive backlogs with rerun guidance. This is the repeatable Phase 3 audit path the ADR asked for.

### 5. Read-path access tracking is only partially complete
`server/crates/djinn-mcp/src/tools/memory_tools/ops.rs` already calls `repo.touch_accessed(&note.id)` and `record_memory_read()` in `memory_read`, but `memory_search` still returns results without touching or recording accessed notes. ADR-054’s access-signal rollout is therefore incomplete.

## Remaining gap versus ADR-054

1. **Access tracking is incomplete for retrieval flows**
   - `memory_read` updates access signals, but `memory_search` does not increment access metadata or co-access tracking for returned results.
   - The final wave should decide the intended semantics for search-result touches and implement them consistently, with tests.

2. **The existing extracted corpus still needs cleanup execution**
   - Audit tooling exists, but the actual migration of pre-existing notes has not been completed.
   - Notes flagged as merge, underspecified, demote, or archive candidates need to be reconciled and rerun against the audit.

3. **Knowledge-base hygiene issues remain around roadmap/design links**
   - Memory health still reports a large broken-link/orphan backlog.
   - At least two broken links remain in `design/`, including one roadmap link and one ADR-title link, so ADR-054 should leave its own planning surfaces cleaner than it found them.

## Wave plan

### Wave 1 — Finish access-signal rollout
- Define and implement the intended access-tracking behavior for `memory_search` and any shared resolve/retrieval helpers implicated by ADR-054.
- Add tests proving access_count / co-access behavior is updated without breaking search semantics.

### Wave 2 — Execute extracted-note corpus cleanup
- Use the extracted-note audit report to reconcile existing `case` / `pattern` / `pitfall` notes.
- Merge duplicate families, strengthen underspecified notes, demote task-local notes, and archive low-value extracted leftovers.
- Rerun the audit and document remaining counts.

### Wave 3 — Repair roadmap/design hygiene surfaced by the audit
- Fix broken roadmap/design wikilinks discovered during memory-health review.
- Ensure the ADR-054 roadmap and related design notes point at canonical targets so the cleanup wave does not leave planning artifacts inconsistent.

## This wave
Create focused worker tasks for:
1. completing MCP memory access-tracking coverage for search/retrieval flows
2. cleaning the extracted corpus using the new ADR-054 audit categories and capturing rerun counts
3. repairing design/roadmap broken links discovered during the ADR-054 audit sweep

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
<<<<<<< HEAD
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]

## Link cleanup note
- Repaired the stale ADR-053 permalink alias above to the canonical target `[[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]`.
- Residual legacy title-alias debt in adjacent design notes was left to the narrower current-note cleanup pass unless a canonical target was unambiguous.
=======
- [[decisions/adr-053-semantic-memory-search-—-candle-embeddings-with-sqlite-vec]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
>>>>>>> origin/main
