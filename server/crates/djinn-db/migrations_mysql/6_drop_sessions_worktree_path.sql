-- Migration 6: Drop sessions.worktree_path.
--
-- Task-run workspace_path has been the source of truth since migration 5
-- + the consumer rewrite in commit bb755ccd1 + the coordinator switchover
-- in commit 63e1800f9. No code path still reads or writes this column.

ALTER TABLE sessions
    DROP COLUMN worktree_path;
