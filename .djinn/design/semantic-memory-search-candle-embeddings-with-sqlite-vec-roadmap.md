---
title: Semantic Memory Search — Candle Embeddings with sqlite-vec Roadmap
type: design
tags: ["semantic-search","memory","candle","sqlite-vec","roadmap"]
---


# Semantic Memory Search — Candle Embeddings with sqlite-vec Roadmap

## Goal
Implement semantic memory search for Djinn by adding in-process embedding inference via candle and vector storage/search via sqlite-vec, while preserving the existing cognitive-memory behavior and graceful fallback to today's FTS-driven retrieval.

## Current State
- Epic `h1yj` is not complete yet.
- The current memory search path is still FTS-only plus existing RRF fusion signals in `server/crates/djinn-db/src/repositories/note/search.rs`.
- MCP `memory_search` still advertises and dispatches an FTS/BM25-only contract in `server/crates/djinn-mcp/src/tools/memory_tools/search.rs` and `server/crates/djinn-mcp/src/tools/memory_tools/ops.rs`.
- The note schema currently has `notes` + `notes_fts` but no vector table or embedding metadata in `server/crates/djinn-db/schema.sql`.
- Note writes and reindexing currently update only the lexical index in `server/crates/djinn-db/src/repositories/note/crud.rs` and `server/crates/djinn-db/src/repositories/note/indexing.rs`.
- DB initialization/migrations currently use embedded refinery migrations and WAL pragmas in `server/crates/djinn-db/src/database.rs` and `server/crates/djinn-db/src/migrations.rs`.

## Wave Plan

### Wave 1 — Foundation seams
1. Add DB support for semantic vectors:
   - migration(s) for embedding metadata and vector storage
   - sqlite-vec extension loading during DB initialization
   - repository seam for upsert/delete/query of note embeddings
2. Add embedding runtime/model seam:
   - candle-backed embedder for note/query text
   - cached model loading and versioning
   - explicit degradation path when model/extension cannot load

### Wave 2 — Retrieval integration
3. Integrate note-write and reindex embedding updates:
   - write path computes embeddings synchronously on successful note writes
   - disk reindex/startup repair path can detect stale/missing embeddings and refresh in the background
4. Integrate semantic search into retrieval:
   - query embedding + vector candidate retrieval
   - merge/dedup with existing FTS candidates before RRF/final ranking
   - preserve useful behavior when semantic search is unavailable

### Wave 3 — Hardening
5. Add focused tests and operator visibility:
   - migration/init tests for extension loading behavior
   - repository tests for vector upsert/query/delete and fallback behavior
   - end-to-end search/write tests proving FTS+semantic merge and graceful degradation

## Task Shape for This Wave
This wave creates five tasks matching the implementation seams above. The dependency chain should be:
- DB/vector foundation and embedder runtime can start first.
- Write-path integration and retrieval integration depend on both foundations.
- Hardening/tests depend on the integration tasks landing.

## Acceptance Gate for Epic Closure
The epic can close only when all of the following are true:
- memory writes persist embeddings (or intentionally no-op with a recorded fallback path)
- startup/background reindex repairs missing or stale embeddings
- `memory_search` runs FTS plus semantic retrieval together and returns merged/deduplicated results
- systems without a working model or sqlite-vec extension degrade cleanly to FTS-only
- tests cover the new initialization, indexing, and retrieval behavior


## Current Wave Status (2026-04-13)
- Foundation task `3tvp` is effectively complete and now sits in `approved` with its acceptance criteria marked met. It added the sqlite-vec schema/migration, DB initialization seam, repository API for note embeddings, and associated tests.
- Foundation task `sljn` is still in progress. The branch already contains the candle embedding runtime/service seam and related snapshot updates, but it is still burning down branch-local verification fallout before it can merge cleanly.
- Follow-on implementation tasks remain the right next wave and are already shaped/sequenced correctly:
  - `z6yv` — embedding updates in note lifecycle + reindex flows
  - `tn0f` — semantic retrieval merged into `memory_search`
  - `l8q4` — verification coverage after implementation lands
- `z6yv` and `tn0f` should stay blocked on both foundations. `l8q4` should stay blocked on both implementation tasks so coverage lands after the code paths stabilize.

## Immediate Next-Wave Focus
1. Land `sljn` without discarding the existing embedding-service implementation.
2. Once both foundations are closed, dispatch `z6yv` to wire embedding maintenance into create/update/delete/reindex flows.
3. Dispatch `tn0f` after or alongside `z6yv` once the shared seams are stable, keeping the public MCP contract unchanged and preserving FTS-only fallback.
4. Dispatch `l8q4` last to consolidate migration/init, lifecycle, retrieval, and degraded-mode verification.

## Closure Readiness Check
The epic is **not** ready to close yet. Remaining closure gates from ADR-053 and this roadmap are still open:
- note lifecycle paths do not yet persist/refresh embeddings end-to-end
- `memory_search` has not yet merged semantic candidates with lexical candidates in the live pipeline
- final hardening/verification for fallback behavior is still pending

## Relations
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[brief]]
