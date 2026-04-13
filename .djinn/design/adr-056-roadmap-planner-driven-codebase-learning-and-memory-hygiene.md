---
title: ADR-056 Roadmap — Planner-Driven Codebase Learning and Memory Hygiene
type: design
tags: ["adr-056","roadmap","planner","memory-hygiene","codebase-learning"]
---

# ADR-056 Roadmap — Planner-Driven Codebase Learning and Memory Hygiene

## Goal
Implement the accepted ADR by extending Planner-led maintenance workflows with actionable memory-health and codebase-learning signals, while keeping the board protected from hygiene-task spam.

## Epic Status
Open. The ADR is accepted/proposed as a work plan, but no implementation tasks beyond this planning task exist yet, and the codebase does not yet show the ADR's glue work: patrol-context enrichment, knowledge-task budgeting, graph-to-note staleness propagation, or planner-created cleanup/exploration flows tied to those signals.

## Existing Foundation Confirmed
- Planner prompt already includes patrol guidance for `memory_health`, broken links, orphan notes, contradictions, and scoped-note review in `server/crates/djinn-agent/src/prompts/planner.md`.
- Housekeeping already prunes associations, flags orphan notes, rebuilds content hashes, and repairs broken wikilinks in `server/src/housekeeping.rs`.
- Duplicate-cluster detection already exists in `server/crates/djinn-db/src/repositories/note/consolidation.rs`.
- Scoped note retrieval and confidence gating already exist in `server/crates/djinn-db/src/repositories/note/search.rs`.
- `STALE_CITATION` confidence scoring already exists in `server/crates/djinn-db/src/repositories/note/scoring.rs`.

## Gaps This Epic Must Close
1. Patrol context does not yet expose duplicate-cluster counts, low-confidence/stale-note counts, or code-graph change summaries in a planner-oriented summary.
2. Planner workflow lacks guard-railed task creation rules for hygiene/exploration triggers described by the ADR.
3. There is no graph-diff → `scope_paths` → stale-confidence/review-needed glue.
4. There is no explicit coverage analysis that cross-references active code structure with scoped-note coverage.

## Wave 1 Plan
Focus on the minimum vertical slice that makes the ADR real without overcommitting to a giant refactor.

### Task 1 — Patrol memory-health enrichment
Add the missing patrol-facing memory-health summary plumbing so Planner can see duplicate clusters, low-confidence notes, stale-note counts, and existing broken-link/orphan signals in one place.

### Task 2 — Patrol knowledge-task budgeting and trigger handling
Wire Planner patrol decision logic so it can create at most a bounded number of knowledge tasks per patrol and avoid flooding the board when open hygiene tasks already exist.

### Task 3 — Code-graph diff + knowledge-gap summary
Expose graph-diff and undocumented-hotspot summaries to Planner patrol context so the Planner can spot structural knowledge gaps before creating architect spikes.

### Task 4 — Freshness decay from changed code
Apply `STALE_CITATION` and `review_needed` tagging to notes whose `scope_paths` overlap changed files from canonical graph refreshes.

### Task 5 — Coverage analysis and regression tests
Add coverage-gap analysis (modules with little/no note coverage vs changed scoped notes) and end-to-end tests proving the planner sees the new summaries and budget behavior.

## Sequencing
- Tasks 1 and 2 establish the patrol-side behavior and should land before patrol begins creating additional work from the new signals.
- Task 3 provides code-graph context that Task 5 will validate.
- Task 4 depends on graph refresh and scoped-note overlap logic and should land after Task 3's graph-side plumbing is available.
- Task 5 closes the wave with coverage analysis and regression proof across the new planner context.

## Done Criteria
The epic can close when Planner patrol has first-class visibility into memory health and code-structure gaps, can create bounded follow-up knowledge tasks from those signals, and code changes automatically reduce confidence/tag scoped notes for review using graph diffs.

## Relations
- [[decisions/adr-056-proposal-planner-driven-codebase-learning-and-memory-hygiene]]
- [[ADR-051: Planner as Patrol and Architect as Consultant]]
- [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]]
- [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]]
