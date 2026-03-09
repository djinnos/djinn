-- Add soft-delete support to activity_log.
-- SQLite supports ADD COLUMN for non-NOT-NULL columns.
ALTER TABLE activity_log ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
