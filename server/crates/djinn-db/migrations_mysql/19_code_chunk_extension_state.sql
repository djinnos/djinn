-- Migration 19: add `extension_state` to `code_chunk_meta` (PR B3 of
-- `~/.claude/plans/code-graph-and-rag-overhaul.md`).
--
-- Mirrors `note_embedding_meta.extension_state` — a tri-valued status
-- token populated by the chunk-and-embed pipeline:
-- * `ready`   — Qdrant upsert succeeded; the vector store actually has
--               a point for this chunk.
-- * `pending` — local meta row written but the Qdrant call failed
--               (collection missing, transient I/O); repair should
--               retry even when the content hash matches.
--
-- Defaults to `pending` so any meta rows pre-PR-B3 (none expected, but
-- the table existed since migration 18) are scheduled for re-embedding
-- on the first warm.

ALTER TABLE code_chunk_meta
    ADD COLUMN extension_state VARCHAR(64) NOT NULL DEFAULT 'pending';
