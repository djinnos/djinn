-- Migration 18: scaffolding for the `code_chunks` collection.
--
-- `code_chunks` holds the per-symbol embedding-text payloads (one row per
-- AST chunk). Mirrors the notes-side split: the heavyweight rendered text
-- lives here, while `code_chunk_meta` carries the small content-hash +
-- model-version fingerprint used for staleness checks.

CREATE TABLE IF NOT EXISTS code_chunks (
    id              VARCHAR(64) NOT NULL PRIMARY KEY,
    project_id      VARCHAR(36) NOT NULL,
    file_path       TEXT        NOT NULL,
    symbol_key      TEXT,
    kind            VARCHAR(32) NOT NULL,
    start_line      INT         NOT NULL,
    end_line        INT         NOT NULL,
    content_hash    VARCHAR(64) NOT NULL,
    embedded_text   TEXT        NOT NULL
);

CREATE INDEX idx_code_chunks_project_file ON code_chunks (project_id, file_path);
CREATE INDEX idx_code_chunks_project_symbol ON code_chunks (project_id, symbol_key);

CREATE TABLE IF NOT EXISTS code_chunk_meta (
    id            VARCHAR(64) NOT NULL PRIMARY KEY,
    project_id    VARCHAR(36) NOT NULL,
    content_hash  VARCHAR(64) NOT NULL,
    model_version VARCHAR(64) NOT NULL,
    embedded_at   VARCHAR(64) NOT NULL
);
