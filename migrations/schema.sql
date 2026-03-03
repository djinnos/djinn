-- Canonical schema — ground truth. Matches result of running all migrations.
-- Updated manually after each migration is added.
-- Last updated: V20260302000001__initial_schema.sql

CREATE TABLE settings (
    key        TEXT NOT NULL PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE projects (
    id         TEXT NOT NULL PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    path       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
