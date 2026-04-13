---
title: Semantic Memory Search — Candle Embeddings with sqlite-vec Roadmap
type: design
tags: ["semantic-search","memory","candle","sqlite-vec","roadmap"]
---


# Semantic Memory Search — Candle Embeddings with sqlite-vec Roadmap

## Goal
Implement semantic memory search for Djinn by adding in-process embedding inference via candle and vector storage/search via sqlite-vec, while preserving the existing cognitive-memory behavior and graceful fallback to today's FTS-driven retrieval.

## Current State
- Epic `h1yj` is still in progress; it is **not** ready for closure yet.
- Wave 1 foundation work is complete: DB/vector storage + sqlite-vec initialization (`3tvp`) and the candle embedding runtime/degradation seam (`sljn`) are closed.
- Wave 2 integration work is in verification: embedding lifecycle/reindex sync (`z6yv`) and semantic retrieval merge (`tn0f`) have both submitted work and reported focused server test passes on their task branches.
- Wave 3 hardening remains the last clear epic gate: verification coverage task `l8q4` is still open and should consolidate migration/init, lifecycle, merged-search, and FTS-only fallback assertions after the two integration branches land.
- An unrelated planner-maintenance PR draft (`e0r1`) is still attached to this epic, so epic closure should wait for board cleanup in addition to semantic-memory verification completion.
- The acceptance gate remains unchanged: close only after the integration branches land, semantic + fallback behavior is verified, and the remaining board noise is resolved.

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

## Relations
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[brief]]