---
title: ADR-056 Roadmap — Planner-Driven Codebase Learning and Memory Hygiene
type: design
tags: ["adr-056","roadmap","planner","memory-hygiene","codebase-learning"]
---

# ADR-056 Roadmap — Planner-Driven Codebase Learning and Memory Hygiene

## Goal
Implement the accepted ADR by extending Planner-led maintenance workflows with actionable memory-health and codebase-learning signals, while keeping the board protected from hygiene-task spam.

## Epic Status
Closed. Wave 1 delivered the planned vertical slice and the codebase now exposes the ADR's core glue: planner-facing patrol context enrichment, guard-railed knowledge-task budgeting, code-graph diff and knowledge-gap summaries, graph-driven scoped-note freshness decay, and combined coverage-analysis/regression proof.

## Completed Foundation and Delivery
- Planner patrol context now surfaces aggregated memory-health signals, including duplicate-cluster counts, low-confidence/stale-note counts, and broken-link/orphan data.
- Planner patrol applies guard rails for knowledge-task budgeting and trigger suppression so patrol can create follow-up work without flooding the board.
- Planner-facing patrol context includes code-graph diff summaries plus undocumented or weakly documented hotspots.
- Canonical graph refresh propagates code-change staleness to scoped notes through `scope_paths` overlap, `STALE_CITATION`, and review-needed marking.
- Coverage analysis and regression coverage prove the combined planner context includes memory-health, code-graph, stale-area, and budgeting signals together.

## Wave 1 Tasks Completed
1. `gpt4` — Implement ADR-056 patrol memory-health summary enrichment.
2. `ncyf` — Implement ADR-056 patrol knowledge-task budgeting and trigger guard rails.
3. `flkb` — Implement ADR-056 code-graph diff and knowledge-gap patrol summary.
4. `tru0` — Implement ADR-056 scoped-note freshness decay from canonical graph changes.
5. `366v` — Implement ADR-056 coverage-gap analysis and planner-context regression tests.

## Outcome Against ADR Scope
The epic's planned phases are functionally satisfied for this wave:
- Phase 1 memory-health visibility: delivered.
- Phase 2 planner-spawn guard rails: delivered.
- Phase 3 code-structure awareness: delivered.
- Phase 4 freshness decay: delivered.
- Phase 5 tuning baseline/regression proof: delivered through combined coverage analysis and tests.

Future tuning can happen as routine follow-up work if patrol behavior reveals threshold or prompt adjustments, but no remaining roadmap items are required to satisfy ADR-056's implementation goal.

## Done Criteria
Satisfied. Planner patrol has first-class visibility into memory health and code-structure gaps, can create bounded follow-up knowledge tasks from those signals, and code changes automatically reduce confidence/tag scoped notes for review using graph diffs.

## Relations
- [[decisions/adr-056-proposal-planner-driven-codebase-learning-and-memory-hygiene]]
- [[ADR-051: Planner as Patrol and Architect as Consultant]]
- [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]]
- [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]]