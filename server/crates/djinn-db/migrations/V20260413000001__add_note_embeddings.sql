-- Foundation for semantic note embeddings.
--
-- `note_embeddings` stores canonical embedding bytes and dimensions even when the
-- sqlite-vec extension is unavailable. The vec0 virtual table is created at
-- runtime during database initialization so startup can gracefully fall back.

CREATE TABLE note_embeddings (
    note_id        TEXT NOT NULL PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE,
    embedding      BLOB NOT NULL,
    embedding_dim  INTEGER NOT NULL,
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE note_embedding_meta (
    note_id         TEXT NOT NULL PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE,
    content_hash    TEXT NOT NULL,
    embedded_at     TEXT NOT NULL,
    model_version   TEXT NOT NULL,
    embedding_dim   INTEGER NOT NULL,
    extension_state TEXT NOT NULL DEFAULT 'pending'
);

CREATE INDEX idx_note_embedding_meta_model_version
    ON note_embedding_meta(model_version);

CREATE INDEX idx_note_embedding_meta_embedded_at
    ON note_embedding_meta(embedded_at DESC);
