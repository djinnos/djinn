-- Migration 6: Drop sessions.worktree_path.
--
-- Task-run workspace_path has been the source of truth since migration 5.
-- No code path still reads or writes this column.

ALTER TABLE sessions
    DROP COLUMN worktree_path;
