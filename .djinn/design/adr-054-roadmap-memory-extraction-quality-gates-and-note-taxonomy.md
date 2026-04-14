---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","extraction","quality-gates"]
---

---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","extraction","quality-gates"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
Epic remains open for the final cleanup/verification stretch. The core ADR-054 implementation is landed: extraction quality-gate decisions, structured durable-note templates, Working Spec routing, the extracted-note audit path, and MCP access-tracking rollout for search/retrieval flows are all complete. The remaining work is operational closure: finish verification/landing of the corpus-cleanup pass and finish verification/landing of the design-link cleanup so ADR-054 leaves the knowledge base in a consistent state.

## Landed work

### 1. Extraction quality-gate decision engine
`server/crates/djinn-agent/src/actors/slot/llm_extraction.rs` evaluates extracted notes across specificity, generality, durability, novelty, type fit, and required structure, then routes them to durable write, merge-into-existing, downgrade-to-working-spec, or discard.

### 2. Working Spec routing
Non-durable but useful extracted knowledge is routed into task-scoped Working Spec `design` notes instead of becoming durable `case` / `pattern` / `pitfall` notes.

### 3. Durable template enforcement
Durable extracted `pattern`, `pitfall`, and `case` notes now require ADR-054 section structure instead of accepting generic one-paragraph notes.

### 4. Extracted-note audit tooling
The repository now has a repeatable audit/reporting path for extracted-note backlog categories: merge, underspecified, demote-to-working-spec, and archive.

### 5. MCP access-tracking rollout
The MCP memory search/retrieval flow task (`wue6`) is closed, so ADR-054's access-signal rollout is complete for the intended read/search boundary.

## Remaining work before epic closure

### A. Corpus cleanup verification and landing (`8vh1`)
The cleanup wave produced a reproducible manifest/script plus evidence for a targeted high-confidence slice of the extracted corpus, including duplicate-family consolidation, underspecified-note rewrites, Working Spec demotions, and archive targets. The task is still in `verifying`, so the epic should stay open until that cleanup result is accepted and landed.

### B. Design/roadmap link cleanup verification and landing (`pd8e`)
The design-link cleanup repaired the ADR-054 roadmap conflict and canonicalized the intended ADR/design targets, but the task is still in `in_task_review`. Keep the epic open until review confirms the cleanup is accepted and landed on main.

## Closure rule for the next planner pass
Close epic `3ch7` immediately once both remaining tasks are closed successfully:
- `8vh1` — Clean the extracted note corpus using ADR-054 audit categories and rerun evidence
- `pd8e` — Repair ADR-054-related design and roadmap broken links with canonical targets

No additional decomposition is needed unless either task is rejected and materially rescoped.

## Current wave assessment
This epic does **not** need another batch of worker tasks right now. The remaining scope is already covered by the two active endgame tasks above, and creating duplicates would expand scope instead of finishing ADR-054.

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
