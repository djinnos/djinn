---
title: ADR-055 Roadmap — Dolt Migration and Per-Task Knowledge Branching
type: design
tags: ["adr-055","roadmap","dolt","qdrant","knowledge-branching"]
---

# ADR-055 Roadmap — Dolt Migration and Per-Task Knowledge Branching

## Goal
Migrate Djinn from SQLite + sqlite-vec to Dolt + Qdrant, then use Dolt branches to isolate per-task knowledge and selectively promote reviewed notes into canonical `main`.

## Current State
The codebase is still SQLite-first:
- `server/crates/djinn-db/src/database.rs` opens `SqlitePool`, applies SQLite pragmas, and initializes `sqlite-vec`.
- `server/crates/djinn-db/schema.sql` defines `notes_fts` as an FTS5 virtual table plus sync triggers.
- `server/crates/djinn-db/src/repositories/note/search.rs` hardcodes FTS5/BM25 queries.
- `server/crates/djinn-db/src/repositories/note/embeddings.rs` stores vectors in SQLite tables plus `note_embeddings_vec`.
- `server/crates/djinn-db/src/repositories/note/crud.rs` already has a useful precursor seam: worktree-scoped note files can be synced into canonical storage, which is conceptually adjacent to future task-branch promotion.

No Dolt, MySQL, or Qdrant implementation seams are present yet.

## Planning Decision
The epic is **not complete**. This wave should establish the first migration seams rather than attempt end-to-end cutover in one step.

## Delivery Strategy

### Wave 1 — Prepare backend seams and independent sidecar work
1. Extract/document the SQLite-specific database/search/vector surfaces that must become backend-aware.
2. Introduce a vector-store abstraction and land a Qdrant-backed implementation behind config while preserving current behavior.
3. Add Dolt/MySQL runtime bootstrap and health-management scaffolding without cutting over the default backend.
4. Prototype/port notes schema and lexical search to a MySQL-compatible path, including FULLTEXT-backed retrieval semantics.
5. Define the task-branch knowledge lifecycle contract so dispatch, extraction, merge, and cleanup hooks can be wired in a later wave without re-planning the data model.

### Wave 2 — Dual-path backend implementation
- Port schema/migrations for operational tables.
- Add repository support for MySQL/Dolt note/task/session storage.
- Wire Qdrant into semantic retrieval and backfill flows.
- Validate search/retrieval parity against current SQLite behavior.

### Wave 3 — Per-task knowledge branching
- Create Dolt branches at dispatch time.
- Bind task/session knowledge writes to the task branch.
- Implement diff, quality-gated promotion, and branch cleanup.
- Add branch-aware Qdrant payload filtering.

### Wave 4 — Operational lifecycle and history features
- Compaction/flatten maintenance flows.
- History/diff/blame tooling surfaces.
- Rollback/admin support and monitoring.

## Wave 1 Task Shape
This wave intentionally favors seam extraction and backend scaffolding over full cutover. The main risk is mixing backend abstraction, schema migration, and branching semantics into one oversized task. Tasks should stay narrowly scoped and use blockers where they touch the same repositories.

## Open Risks
- MySQL FULLTEXT quality may not match current FTS5 scoring closely enough without additional tuning.
- Dolt SQL procedure semantics may require transactional patterns different from current `sqlx` repository assumptions.
- Branch-aware semantic retrieval needs payload/filter design in Qdrant before cutover.

## Relations
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
