-- Knowledge base notes schema.
--
-- Notes are markdown files on disk; this table is the search index.
-- Notes are scoped to a project (project_id FK to projects).
-- FTS5 virtual table provides BM25-ranked full-text search.

CREATE TABLE notes (
    id            TEXT NOT NULL PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    permalink     TEXT NOT NULL,    -- slug path e.g. "decisions/my-adr"
    title         TEXT NOT NULL,
    file_path     TEXT NOT NULL,    -- absolute path to .md file on disk
    note_type     TEXT NOT NULL DEFAULT '',
    folder        TEXT NOT NULL DEFAULT '',
    tags          TEXT NOT NULL DEFAULT '[]', -- JSON array
    content       TEXT NOT NULL DEFAULT '',   -- markdown body (no frontmatter)
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_accessed TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (project_id, permalink)
);

CREATE INDEX notes_project_id ON notes(project_id);
CREATE INDEX notes_folder     ON notes(folder);
CREATE INDEX notes_type       ON notes(note_type);
CREATE INDEX notes_updated_at ON notes(updated_at);

-- FTS5 external content table for BM25-ranked full-text search.
-- content='notes' tells FTS5 to retrieve content from the notes table.
-- content_rowid='rowid' maps FTS rowids to notes implicit rowids.
CREATE VIRTUAL TABLE notes_fts USING fts5(
    title,
    content,
    tags,
    content='notes',
    content_rowid='rowid',
    tokenize='unicode61'
);

-- Triggers keep the FTS5 index in sync with notes rows.
CREATE TRIGGER notes_fts_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER notes_fts_au AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
    INSERT INTO notes_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER notes_fts_ad AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
END;
