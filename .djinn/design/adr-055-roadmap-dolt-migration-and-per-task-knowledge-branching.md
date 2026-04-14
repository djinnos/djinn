---
title: ADR-055 Roadmap: Dolt Migration and Per-Task Knowledge Branching
type: design
tags: ["adr-055","roadmap","dolt","mysql","qdrant"]
---


# ADR-055 Roadmap — Dolt Migration and Per-Task Knowledge Branching


## Status

Epic `5izw` is complete and ready for closure. Wave 1 and Wave 2 both landed: the codebase now has executable MySQL/Dolt runtime support, backend-aware lexical search execution, Dolt server/branch lifecycle helpers, SQLite→Dolt import verification tooling, and coordinator history-maintenance hooks on top of the earlier schema, Qdrant, and branch-aware knowledge seams.

The roadmap work described in this note has been fully delivered; no additional decomposition wave is required for ADR-055.

## Completed in Wave 2

- Real MySQL/Dolt runtime support landed in `djinn-db`, replacing the earlier staging-error path.
- Repository lexical search now routes through the backend-aware execution seam for SQLite and MySQL/Dolt.
- Dolt server-management and branch SQL lifecycle helpers landed for branch create / checkout / merge / delete operations.
- SQLite-to-Dolt migration import verification tooling now provides reproducible import checks and dry-run safety guidance.
- Coordinator maintenance now includes ADR-055 history-maintenance / compaction scheduling hooks with safety guards.

## Remaining work

None for this epic. Any future follow-up should be opened as a new epic focused on production cutover hardening or operational rollout rather than additional ADR-055 decomposition.

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
