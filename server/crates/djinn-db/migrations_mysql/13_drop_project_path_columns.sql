-- Migration 13: drop `path` and `clone_path` columns; promote
-- `(github_owner, github_repo)` to the primary project identity.
--
-- Rationale: a single persisted filesystem path doesn't round-trip
-- across the server pod, architect pod, and worker pods that each
-- mount the projects volume at their own location. Path is a runtime
-- derivation of `$DJINN_HOME/projects/{owner}/{repo}`, not persisted
-- state. Removing the column forces every caller to synthesize it
-- locally and eliminates an entire class of "but whose path is this?"
-- bugs.
--
-- Post-migration invariant: every project row has a non-empty
-- `github_owner` + `github_repo`. Legacy non-GitHub projects are not
-- supported in the K8s deployment and the column NOT NULL promotion
-- is safe because `project_add_from_github` is the only creation path.
--
-- The UNIQUE (github_owner, github_repo) constraint (added in
-- migration 2) already exists and becomes the natural key.

-- 1. Drop the unique index on path first so the column drop succeeds.
ALTER TABLE projects DROP INDEX uq_projects_path;

-- 2. Drop the columns themselves.
ALTER TABLE projects DROP COLUMN path;
ALTER TABLE projects DROP COLUMN clone_path;

-- 3. Promote github coords to NOT NULL. Any row that would violate
--    this has been orphaned by the K8s pivot and should have been
--    cleaned up before this migration runs.
ALTER TABLE projects MODIFY COLUMN github_owner VARCHAR(255) NOT NULL;
ALTER TABLE projects MODIFY COLUMN github_repo  VARCHAR(255) NOT NULL;
