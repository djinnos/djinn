-- Wikilink edges between knowledge base notes.
--
-- Each [[Target]] in note content becomes one row here.
-- source_id → target note that contains the link.
-- target_id → resolved note (NULL = broken link, target doesn't exist).
-- Rows cascade-delete when the source note is deleted.
-- target_id is SET NULL when the target note is deleted.

CREATE TABLE note_links (
    id           TEXT NOT NULL PRIMARY KEY,
    source_id    TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    target_id    TEXT REFERENCES notes(id) ON DELETE SET NULL,
    target_raw   TEXT NOT NULL,   -- raw text inside [[...]] (before pipe)
    display_text TEXT,            -- alias text after | if present
    UNIQUE (source_id, target_raw)
);

CREATE INDEX note_links_source ON note_links(source_id);
CREATE INDEX note_links_target ON note_links(target_id);
