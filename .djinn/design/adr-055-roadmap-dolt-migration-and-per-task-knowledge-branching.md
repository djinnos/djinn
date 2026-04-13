---
title: ADR-055 roadmap — Dolt migration and per-task knowledge branching
type: design
tags: ["adr-055","roadmap","dolt","mysql","qdrant","branching"]
---

# ADR-055 roadmap — Dolt migration and per-task knowledge branching

Originated from epic `5izw` and task `019d8915-2695-7593-83a9-4226231a6675` (`mved`).

## Status

Epic remains open. Wave 1 backend-seam work is active and not yet complete, so the epic is not ready for closure.

This roadmap tracks Wave 1 decomposition for [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]].

## Architectural goal

Implement [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]] by replacing SQLite-specific note storage/search seams with Dolt + MySQL-compatible relational storage, Qdrant-backed vector retrieval, and a per-task knowledge-branch lifecycle that isolates speculative extraction until promotion.

## Wave 1 goals

1. Catalog SQLite-specific seams so follow-on work can land behind explicit boundaries.
2. Introduce a vector-store abstraction and Qdrant scaffold.
3. Refactor database bootstrap so SQLite and Dolt/MySQL backends can coexist during migration.
4. Prototype MySQL/Dolt lexical search to replace FTS5.
5. Define the task-branch knowledge lifecycle contract.

### In flight / planned
- `mved` — catalog SQLite-specific migration seams for Dolt/MySQL + Qdrant
- `6iiz` — introduce vector-store abstraction and Qdrant scaffold for note embeddings
- `y70d` — refactor database bootstrap for selectable SQLite vs Dolt/MySQL backends
- `keit` — prototype Dolt/MySQL FULLTEXT notes search to replace FTS5
- `4hkv` — design task-branch knowledge promotion flow for ADR-055
  - output: [[design/adr-055-task-branch-knowledge-promotion-flow]]

## Active work buckets

### 1. Seam inventory and migration map

- `mved` — catalog SQLite-specific migration seams for ADR-055.
- Output note: [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]].
- This note is now the canonical inventory of SQLite-coupled bootstrap, migration, FTS5, sqlite-vec, pragma, and SQL-dialect seams.

### 2. Vector-store extraction

- `6iiz` — introduce vector-store abstraction and Qdrant scaffold for note embeddings.
- Should follow the `VectorStore` and embedding-metadata split recommended in [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]].

### 3. Relational backend/bootstrap extraction

- `y70d` — refactor database bootstrap for selectable SQLite vs Dolt/MySQL backends.
- Should use the `DatabaseBackend` / `SchemaMigrator` seam identified in [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]].

### 4. Lexical search replacement

- `keit` — prototype Dolt/MySQL FULLTEXT notes search to replace FTS5.
- Should keep RRF fusion but replace the lexical candidate backend described in [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]].

### 5. Task-branch knowledge promotion flow

- `4hkv` — design task-branch knowledge promotion flow for ADR-055.
- Should assume branch checkout and branch-aware reads/writes live below repositories in the backend/session layer, per [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]].

## What Wave 1 must answer

1. Where SQLite-specific assumptions currently live across `database.rs`, note search, embeddings, migration/bootstrap, and note sync.
2. What backend seams are required so SQLite remains functional while Dolt/Qdrant scaffolding lands.
3. How task dispatch, session extraction, branch-scoped note IO, promotion review, and cleanup should interact under Dolt task branches.

## Concrete code seams confirmed during planning

- `server/crates/djinn-db/src/database.rs` is tightly coupled to SQLite pool types, pragma setup, and sqlite-vec initialization.
- `server/crates/djinn-db/src/repositories/note/search.rs` depends on FTS5 tables and BM25-specific SQL query shape.
- `server/crates/djinn-db/src/repositories/note/embeddings.rs` assumes sqlite-vec availability and local embedding persistence semantics.
- `server/crates/djinn-db/src/repositories/note/crud.rs` already contains worktree `.djinn/` note parsing and file-backed note sync behavior that must be reconciled with branch-aware canonical storage.
- `server/crates/djinn-agent/src/actors/coordinator/dispatch.rs`, `server/crates/djinn-agent/src/actors/slot/session_extraction.rs`, and `server/crates/djinn-agent/src/actors/slot/llm_extraction.rs` are the key lifecycle seams for branch creation, branch-scoped extraction writes, and cleanup/promotion triggers.

## Initial sequencing

1. Land seam inventory.
2. Extract vector-store and backend bootstrap seams.
3. Prototype FULLTEXT lexical replacement behind a lexical backend boundary.
4. Define branch lifecycle contract once backend/session ownership is explicit.
5. Start Wave 2 implementation only after Wave 1 seam outputs are merged.

## Next wave (Wave 2) — implementation after Wave 1 lands

Wave 2 should not start until the seam inventory, backend bootstrap seam, lexical-search prototype, vector-store seam, and branch-flow contract exist. The pre-created follow-on tasks are blocked on Wave 1 outputs so they are ready once the foundation is merged.

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
