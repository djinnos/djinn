-- Migration 13: drop `path` and `clone_path` columns; promote
-- `(github_owner, github_repo)` to the primary project identity.
--
-- Path is a runtime derivation of `$DJINN_HOME/projects/{owner}/{repo}`,
-- not persisted state. Removing the column forces every caller to
-- synthesize it locally and eliminates an entire class of "but whose
-- path is this?" bugs.

-- 1. Drop the unique constraint on path first so the column drop succeeds.
ALTER TABLE projects DROP CONSTRAINT uq_projects_path;

-- 2. Drop the columns themselves.
ALTER TABLE projects DROP COLUMN path;
ALTER TABLE projects DROP COLUMN clone_path;

-- 3. Promote github coords to NOT NULL. Any row that would violate
--    this has been orphaned by the K8s pivot and should have been
--    cleaned up before this migration runs.
ALTER TABLE projects ALTER COLUMN github_owner SET NOT NULL;
ALTER TABLE projects ALTER COLUMN github_repo  SET NOT NULL;
