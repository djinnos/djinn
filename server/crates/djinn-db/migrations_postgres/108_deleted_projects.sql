-- Retain a minimal tombstone after a project is hard-deleted. This lets
-- cache-maintenance code distinguish a former project warm base from an
-- arbitrary UUID-named directory without retaining the project itself.
CREATE TABLE deleted_projects (
    project_id VARCHAR(36) NOT NULL PRIMARY KEY,
    deleted_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);
