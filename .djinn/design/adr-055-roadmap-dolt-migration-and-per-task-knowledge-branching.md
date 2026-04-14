---
title: ADR-055 Roadmap: Dolt Migration and Per-Task Knowledge Branching
type: design
tags: ["adr-055","roadmap","dolt","mysql","qdrant"]
---

# ADR-055 Roadmap — Dolt Migration and Per-Task Knowledge Branching

## Status

Epic `5izw` remains open. Wave 1 is complete: the codebase now has explicit seams for selectable SQLite vs MySQL/Dolt backend configuration, staged MySQL schema artifacts, backend-neutral lexical-search planning, Qdrant vector-store scaffolding, and branch-aware knowledge promotion/retrieval seams.

The next work should move from scaffolding into executable runtime cutover infrastructure while preserving SQLite as the safe default.

## Completed in Wave 1

- SQLite-coupled migration seams inventoried in [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]].
- Vector storage abstraction introduced with Qdrant scaffold and branch-aware embedding metadata/filtering.
- Database bootstrap refactored to make backend selection explicit for `sqlite`, `mysql`, and `dolt`.
- MySQL FULLTEXT replacement path documented and encoded in backend-neutral lexical-search planning seams.
- MySQL/Dolt schema snapshot and migration-plan docs added under `server/crates/djinn-db/docs/` and `server/crates/djinn-db/sql/`.
- Task-session lifecycle, extraction writes, promotion, and cleanup now route through branch-aware knowledge seams.

## Evidence from current codebase

- `server/crates/djinn-db/src/database.rs` exposes explicit backend selection, but MySQL/Dolt still returns a staging error rather than opening a real runtime.
- `server/crates/djinn-db/src/repositories/note/lexical_search.rs` defines the MySQL FULLTEXT plan shape, but repository execution still needs to consume that plan when the MySQL backend is live.
- `server/crates/djinn-db/docs/adr-055-schema-migration-plan.md` and `server/crates/djinn-db/sql/mysql_schema.sql` provide the relational target, but backend-specific migrators and import verification remain open.
- ADR-055 Phase 4 lifecycle work (compaction / flatten / GC guardrails) is still only described in the ADR and has not been wired into coordinator maintenance.

## Wave 2 — Executable MySQL/Dolt runtime

### Goal

Make the staged Dolt/MySQL path runnable in the server while keeping SQLite as the default production path.

### Planned tasks

1. **Implement real MySQL/Dolt connection runtime and repository execution seam**
   - Replace the current explicit staging error for `mysql` / `dolt` backend selection with a real `sqlx` MySQL pool path.
   - Define how repository code branches on backend capabilities without duplicating all higher-level application logic.

2. **Cut lexical note search over to backend-aware execution**
   - Consume the existing lexical-search planning seam in live repository queries.
   - Preserve current SQLite behavior while enabling MySQL FULLTEXT execution once the MySQL runtime exists.

3. **Add Dolt server manager and branch SQL lifecycle helpers**
   - Manage `dolt sql-server` startup/health checks.
   - Provide helper seams for branch create / checkout / merge / delete and session-scoped branch checkout.

4. **Build SQLite → Dolt migration/import verification tooling**
   - Add a reproducible export/import path and row-count verification against the staged MySQL schema target.
   - Make rollback and dry-run expectations explicit.

5. **Wire ADR-055 lifecycle maintenance into coordinator operations**
   - Implement compaction / flatten planning and safety checks once executable Dolt runtime support exists.

## Sequencing

- Runtime support must land before live MySQL lexical search execution.
- Dolt server-management and branch SQL helpers depend on executable MySQL/Dolt connectivity.
- Migration/import tooling should target the same runtime/config seams to avoid a throwaway path.
- Lifecycle maintenance should follow once the system can actually execute Dolt procedures.

## Deferred beyond this wave

- Full production cutover from SQLite default to Dolt default.
- Empirical threshold retuning for MySQL FULLTEXT relevance once real runtime data is available.
- Operational packaging for Dolt + Qdrant services across all deployment modes.

## Relations

- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]]
