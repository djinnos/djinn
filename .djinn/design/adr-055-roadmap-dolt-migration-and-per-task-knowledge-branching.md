---
title: ADR-055 roadmap — Dolt migration and per-task knowledge branching
type: design
tags: ["adr-055","roadmap","dolt","mysql","qdrant","branching"]
---

# ADR-055 roadmap — Dolt migration and per-task knowledge branching

Originated from epic `5izw` and task `019d8915-2695-7593-83a9-4226231a6675` (`mved`).

## Status

Proposed.

This roadmap tracks Wave 1 decomposition for [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]].

## Wave 1 goals

1. Catalog SQLite-specific seams so follow-on work can land behind explicit boundaries.
2. Introduce a vector-store abstraction and Qdrant scaffold.
3. Refactor database bootstrap so SQLite and Dolt/MySQL backends can coexist during migration.
4. Prototype MySQL/Dolt lexical search to replace FTS5.
5. Define the task-branch knowledge lifecycle contract.

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

## Initial sequencing

1. Land seam inventory.
2. Extract vector-store and backend bootstrap seams.
3. Prototype FULLTEXT lexical replacement behind a lexical backend boundary.
4. Define branch lifecycle contract once backend/session ownership is explicit.

## Relations

- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]]
