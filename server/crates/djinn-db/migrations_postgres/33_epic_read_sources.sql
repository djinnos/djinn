-- Read-only multi-repo support: an epic may declare other registered projects
-- whose source/knowledge its tasks are allowed to READ (writes stay pinned to
-- the epic's own project). Scoped per-epic (not per-project) so the grant is
-- bounded by the work and disappears when the epic is deleted (ON DELETE
-- CASCADE on both sides). The canonical case is a migration epic targeting
-- repo B that must read legacy repo A.
CREATE TABLE IF NOT EXISTS epic_read_sources (
    epic_id     VARCHAR(36) NOT NULL,
    project_id  VARCHAR(36) NOT NULL,
    created_at  VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (epic_id, project_id),
    CONSTRAINT fk_ers_epic    FOREIGN KEY (epic_id)    REFERENCES epics(id)    ON DELETE CASCADE,
    CONSTRAINT fk_ers_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX epic_read_sources_epic_id ON epic_read_sources(epic_id);
