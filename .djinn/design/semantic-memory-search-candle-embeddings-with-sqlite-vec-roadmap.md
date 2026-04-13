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

## Maintenance Note
This roadmap note was re-written through `memory_write` after patrol found artifact drift: the markdown file already existed under `design/semantic-memory-search-candle-embeddings-with-sqlite-vec-roadmap`, but `memory_read` could not resolve it. The canonical permalink remains `design/semantic-memory-search-candle-embeddings-with-sqlite-vec-roadmap`; references should keep using this permalink rather than creating a duplicate note.


## Semantic-memory orphan cleanup batch (2026-04-13)

This roadmap now serves as the canonical planning surface for the active semantic-memory case cluster identified during orphan triage. The high-value implementation cases that should remain as searchable supporting notes and be linked from active planning are:

- [[cases/add-note-embedding-storage-schema-alongside-sqlite-vec-virtual-table-support]] — keep and link
- [[cases/create-a-database-initialization-seam-for-optional-sqlite-vec-enablement]] — keep and link
- [[cases/embedding-runtime-seam-added-for-semantic-memory]] — keep and link
- [[cases/keep-note-embeddings-synchronized-during-write-update-and-delete-flows]] — keep and link
- [[cases/repair-stale-note-embeddings-during-reindex-and-background-maintenance]] — keep and link
- [[cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface]] — keep and link
- [[cases/memory-search-tests-extended-for-semantic-retrieval-without-changing-the-public-contract]] — keep and link

The following nearby orphan cases are treated as consolidation/deprecation candidates rather than separate canonical references because their guidance is now absorbed by the survivors above or by this roadmap/[[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]:

- `cases/blend-semantic-retrieval-into-existing-note-search-without-changing-the-mcp-interface` → consolidate into `cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface`
- `cases/blend-semantic-vector-search-into-existing-memory-search-ranking` → consolidate into `cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface`
- `cases/added-vector-aware-ranking-to-the-existing-note-search-pipeline` → consolidate into `cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface`
- `cases/thread-semantic-search-context-through-bridge-and-state-layers` → consolidate into `cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface`
- `cases/propagate-embedding-aware-memory-behavior-through-mcp-and-bridge-layers` → consolidate into the write/reindex survivors above
- `cases/restore-task-verification-by-preserving-embedding-runtime-and-updating-downstream-schema-artifacts` and `cases/restore-task-scoped-verification-without-disturbing-embedding-runtime-changes` → de-emphasize as task-local verification repair notes, not canonical semantic-memory references

Patrol guidance: future orphan review should treat the survivor set above as the active semantic-memory knowledge slice. The consolidation candidates should not be prioritized again unless their content diverges from the surviving notes.
