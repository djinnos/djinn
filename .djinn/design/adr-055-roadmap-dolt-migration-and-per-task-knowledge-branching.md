---
title: ADR-055 Roadmap — Dolt Migration and Per-Task Knowledge Branching
type: design
tags: ["adr-055","roadmap","dolt","qdrant","knowledge-branching"]
---

# ADR-055 Roadmap — Dolt Migration and Per-Task Knowledge Branching

## Status
Epic remains open. Wave 1 backend-seam work is active and not yet complete, so the epic is not ready for closure.

## Architectural goal
Implement [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]] by replacing SQLite-specific note storage/search seams with Dolt + MySQL-compatible relational storage, Qdrant-backed vector retrieval, and a per-task knowledge-branch lifecycle that isolates speculative extraction until promotion.

## Current wave (Wave 1) — foundation and uncertainty reduction

### In flight / planned
- `mved` — catalog SQLite-specific migration seams for Dolt/MySQL + Qdrant
- `6iiz` — introduce vector-store abstraction and Qdrant scaffold for note embeddings
- `y70d` — refactor database bootstrap for selectable SQLite vs Dolt/MySQL backends
- `keit` — prototype Dolt/MySQL FULLTEXT notes search to replace FTS5
- `4hkv` — design task-branch knowledge promotion flow for ADR-055
  - output: [[design/adr-055-task-branch-knowledge-promotion-flow]]

### What Wave 1 must answer
1. Where SQLite-specific assumptions currently live (`database.rs`, note search, embeddings, migration/bootstrap, worktree note sync).
2. What backend seams are required so SQLite remains functional while Dolt/Qdrant scaffolding lands.
3. How task dispatch, session extraction, branch-scoped note IO, promotion review, and cleanup should interact under Dolt task branches.

### Concrete code seams confirmed during planning
- `server/crates/djinn-db/src/database.rs` is tightly coupled to SQLite pool types, pragma setup, and sqlite-vec initialization.
- `server/crates/djinn-db/src/repositories/note/search.rs` depends on FTS5 tables and BM25-specific SQL query shape.
- `server/crates/djinn-db/src/repositories/note/embeddings.rs` assumes sqlite-vec availability and local embedding persistence semantics.
- `server/crates/djinn-db/src/repositories/note/crud.rs` already contains worktree `.djinn/` note parsing and file-backed note sync behavior that must be reconciled with branch-aware canonical storage.
- `server/crates/djinn-agent/src/actors/coordinator/dispatch.rs`, `server/crates/djinn-agent/src/actors/slot/session_extraction.rs`, and `server/crates/djinn-agent/src/actors/slot/llm_extraction.rs` are the key lifecycle seams for branch creation, branch-scoped extraction writes, and cleanup/promotion triggers.

## Next wave (Wave 2) — implementation after Wave 1 lands
Wave 2 should not start until the seam inventory, backend bootstrap seam, lexical-search prototype, vector-store seam, and branch-flow contract exist. The tasks below are pre-created and blocked on Wave 1 outputs so they are ready once the foundation is merged.

### Wave 2 tasks
1. MySQL/Dolt schema and migration port for note/task storage.
2. Branch-aware session database routing plus task-branch lifecycle hooks.
3. Promotion-review execution flow that diffs task branches, applies quality gates, and cleans up branch/Qdrant residue.
4. Branch-aware embedding sync and retrieval filtering across `main` + task branches.

## Closure check
Do not close this epic until:
- Wave 1 tasks are reviewed and merged.
- Wave 2 migration tasks are complete.
- The codebase has an end-to-end task-branch knowledge flow, not just scaffolding.

## Relations
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]]
