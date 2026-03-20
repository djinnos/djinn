-- ADR-030: Commands now live in .djinn/settings.json, not in the DB.
-- SQLite doesn't support DROP COLUMN before 3.35.0; use table rebuild.
CREATE TABLE projects_new (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    path        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    target_branch TEXT NOT NULL DEFAULT 'main',
    auto_merge    INTEGER NOT NULL DEFAULT 0,
    sync_enabled  INTEGER NOT NULL DEFAULT 0,
    sync_remote   TEXT
);

INSERT INTO projects_new (id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote)
    SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote
    FROM projects;

DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;
