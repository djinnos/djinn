-- Migration 19: add `extension_state` to `code_chunk_meta`.
--
-- Mirrors `note_embedding_meta.extension_state` — a tri-valued status
-- token populated by the chunk-and-embed pipeline:
-- * `ready`   — Qdrant upsert succeeded.
-- * `pending` — local meta row written but the Qdrant call failed.

ALTER TABLE code_chunk_meta
    ADD COLUMN extension_state VARCHAR(64) NOT NULL DEFAULT 'pending';
