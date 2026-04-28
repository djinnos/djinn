-- Migration 18: scaffolding for the `code_chunks` collection (Epic B / PR B1
-- of `~/.claude/plans/code-graph-and-rag-overhaul.md`). The chunker (B2),
-- embedding pipeline (B3), and hybrid retrieval (B4) land in follow-ups;
-- this migration only stands up the storage so those PRs land cleanly.
--
-- `code_chunks` holds the per-symbol embedding-text payloads (one row per
-- AST chunk). Mirrors the notes-side split: the heavyweight rendered text
-- lives here, while `code_chunk_meta` carries the small content-hash +
-- model-version fingerprint used for staleness checks. Keeping them
-- separate matches the `note_embeddings` / `note_embedding_meta` shape
-- and lets the repair tool walk meta rows cheaply without dragging
-- MEDIUMTEXT bodies across the wire.

CREATE TABLE IF NOT EXISTS code_chunks (
    id              VARCHAR(64) NOT NULL,
    project_id      VARCHAR(36) NOT NULL,
    file_path       TEXT        NOT NULL,
    symbol_key      TEXT,
    kind            VARCHAR(32) NOT NULL,
    start_line      INT         NOT NULL,
    end_line        INT         NOT NULL,
    content_hash    VARCHAR(64) NOT NULL,
    embedded_text   MEDIUMTEXT  NOT NULL,
    PRIMARY KEY (id),
    KEY idx_project_file (project_id, file_path(255)),
    KEY idx_project_symbol (project_id, symbol_key(255))
);

CREATE TABLE IF NOT EXISTS code_chunk_meta (
    id            VARCHAR(64) NOT NULL,
    project_id    VARCHAR(36) NOT NULL,
    content_hash  VARCHAR(64) NOT NULL,
    model_version VARCHAR(64) NOT NULL,
    embedded_at   VARCHAR(64) NOT NULL,
    PRIMARY KEY (id)
);
