---
title: ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec
type: adr
tags: ["adr","memory","embeddings","candle","sqlite-vec","nomic","semantic-search"]
---


# ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec

## Status: Proposed

Date: 2026-04-13

Related: [[decisions/adr-023-cognitive-memory-architecture-multi-signal-retrieval-and-associative-learning|ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]], [[decisions/adr-042-db-only-knowledge-extraction-consolidation-and-task-routing-fixes|ADR-042: DB-Only Knowledge Extraction, Consolidation, and Task-Routing Fixes]]

## Context

The memory system currently holds ~800 notes across patterns, cases, pitfalls, reference, and decisions. Search relies on keyword/full-text matching, which works when the agent knows the exact terms but fails on semantic queries like "what do we know about task routing edge cases" when the relevant notes use words like "dispatch", "slot", or "RoleRegistry".

As the knowledge base grows, this gap widens. Agents waste tokens grepping, reading wrong files, grepping again. Embeddings would let agents find notes by *meaning* rather than exact keywords, covering the ~20% of lookups where keyword search falls short.

A secondary motivation is moving toward a filesystem-first memory interface where agents use standard `Read`/`Write`/`Grep`/`Glob` tools for most operations, with semantic search as a thin acceleration layer rather than a full MCP API replacement.

## Decision

Add semantic vector search to the memory system using:

- **Embedding model**: `nomic-embed-text-v1.5` (~137M params, 768-dim vectors) loaded in-process via the **`candle`** crate (Hugging Face's Rust ML framework). No Python, no external process, no API dependency.
- **Vector storage**: **`sqlite-vec`** extension for the existing SQLite database. Vectors are stored alongside note metadata in a virtual table, keeping everything in one DB file.
- **Computation**: Embeddings are computed **synchronously** on write. At 10-50ms per note, the latency is negligible and avoids the complexity of async pipelines, eventual consistency, and stale detection.

### Architecture

```
memory_write() ──→ persist note to DB
               ──→ compute embedding via candle (10-50ms)
               ──→ upsert vector into sqlite-vec
               ──→ done

memory_search(query) ──→ embed query string (~10-50ms)
                     ──→ run FTS + sqlite-vec semantic search in parallel
                     ──→ merge, deduplicate, rank by combined score
                     ──→ return note paths + scores

startup/reindex ──→ background task: walk all notes
                ──→ compare content_hash, re-embed stale/missing
                ──→ ~40s for full corpus, doesn't block daemon
```

Every search always runs **both** FTS and semantic, merging results. This avoids the impossible problem of knowing what semantic search missed — FTS catches exact term matches and newly indexed notes, semantic catches meaning-based matches, and the union covers both.

### Schema

```sql
-- sqlite-vec virtual table
CREATE VIRTUAL TABLE memory_vectors USING vec0(
    note_id TEXT PRIMARY KEY,
    embedding float[768]
);

-- metadata for index management
CREATE TABLE memory_embedding_meta (
    note_id    TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,  -- detect stale embeddings on reindex
    embedded_at  TEXT NOT NULL,  -- ISO 8601
    model_version TEXT NOT NULL  -- track model changes for reindexing
);
```

### Model management

- The nomic-embed-text safetensors weights (~250MB) are downloaded on first use and cached in `~/.djinn/models/` (or a configurable path).
- `model_version` in the meta table tracks which model version produced each embedding. If the model is upgraded, all embeddings are automatically re-computed on next startup via the background reindex task.
- For systems without enough RAM or CPU, semantic search degrades gracefully to FTS-only — the embedding model simply doesn't load, and search falls back to keyword matching.

## Alternatives considered

### Qdrant (embedded via qdrant-segment)
Heavy dependency — pulls in the full Qdrant storage engine. More than needed for <10K vectors. sqlite-vec keeps everything in the existing DB.

### usearch / hnsw_rs
Lightweight vector index libraries, but they require a separate index file and don't integrate with our existing SQLite storage. At our scale (~800 vectors), sqlite-vec's brute-force scan is fast enough, and HNSW's advantages only matter at 100K+ vectors.

### External embedding API (OpenAI, Voyage)
Better embedding quality, but adds a network dependency, API key requirement, and per-token cost. Conflicts with Djinn's self-hosted, offline-capable design. Can be offered as an optional alternative later.

### Ollama API
Good middle ground if the user already runs Ollama, but not embeddable — requires a separate running process. Could be offered as a fallback for users who already have Ollama and don't want candle's memory overhead.

### Async embedding pipeline
Considered computing embeddings in a background worker pool via bounded channels to avoid blocking writes. Rejected because: (a) 10-50ms inline latency is negligible, (b) async introduces eventual consistency where newly written notes aren't semantically searchable yet, (c) the "fallback to FTS" for unembedded notes is a false safety net — you can't know what semantic search would have found, and (d) the pipeline complexity (channels, workers, stale detection) isn't justified at this scale.

## Consequences

### Positive
- Memory search quality improves significantly for semantic/fuzzy queries
- Zero external dependencies — everything runs in-process
- Synchronous embedding keeps the system simple and always consistent
- Combined FTS + semantic search covers both exact and fuzzy queries
- sqlite-vec keeps the single-DB-file deployment model
- Path toward filesystem-first memory: semantic search returns file paths, agents use standard tools from there

### Negative
- ~250MB model download on first use
- ~200-400MB additional RAM when the embedding model is loaded
- 10-50ms added latency per write (negligible in practice)
- CPU cost during bulk reindexing on startup (~40s for full corpus)
- candle adds compile-time complexity and binary size

### Risks
- candle is younger than PyTorch/ONNX Runtime — may hit edge cases with nomic model loading
- sqlite-vec is relatively new — need to verify concurrent read/write behavior under WAL mode
- nomic-embed-text quality on code-heavy content (stack traces, function names) is unvalidated — may need a code-specific model later

## Migration

No schema migration needed for existing notes — the embedding table is additive. On first startup after deployment, the background reindex task embeds all existing notes. At ~800 notes and ~50ms per embedding, full reindex takes ~40 seconds and runs without blocking the daemon.
