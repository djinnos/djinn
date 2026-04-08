-- ADR-051 Epic C — Proposal pipeline backend.
--
-- Adds:
--   1. A new 'proposed' epic status.  Epics in this state are architect
--      drafts that must not trigger auto-dispatch until explicitly
--      accepted (see coordinator::wave::maybe_create_planning_task and
--      the propose_adr_accept MCP tool).
--   2. auto_breakdown (INTEGER 0/1): when 0, epic_created no longer
--      triggers an automatic breakdown Planner dispatch.  Default 1 to
--      preserve existing behaviour.
--   3. originating_adr_id (TEXT, nullable): slug of the accepted ADR
--      that spawned this epic, threaded through into the breakdown
--      Planner's session context so downstream task creation inherits
--      the rationale.
--
-- SQLite does not support altering CHECK constraints in-place, so we
-- rebuild the table (mirroring V20260327000001__epic_add_drafting_status).

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS epics_new;
CREATE TABLE epics_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    emoji               TEXT NOT NULL DEFAULT '',
    color               TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'drafting'
                             CHECK(status IN ('proposed', 'drafting', 'open', 'closed')),
    owner               TEXT NOT NULL DEFAULT '',
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    auto_breakdown      INTEGER NOT NULL DEFAULT 1 CHECK(auto_breakdown IN (0, 1)),
    originating_adr_id  TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO epics_new (
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs,
    auto_breakdown, originating_adr_id
)
SELECT
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs,
    1, NULL
FROM epics;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

PRAGMA foreign_keys = ON;
