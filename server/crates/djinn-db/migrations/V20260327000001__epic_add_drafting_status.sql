-- Add 'drafting' status to epics. New epics default to 'drafting'.
-- Existing open epics remain open (no data migration needed).

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS epics_new;
CREATE TABLE epics_new (
    id          TEXT NOT NULL PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id    TEXT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    emoji       TEXT NOT NULL DEFAULT '',
    color       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'drafting'
                     CHECK(status IN ('drafting', 'open', 'closed')),
    owner       TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at   TEXT,
    memory_refs TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, short_id)
);

INSERT INTO epics_new (
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs
)
SELECT
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs
FROM epics;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

PRAGMA foreign_keys = ON;
