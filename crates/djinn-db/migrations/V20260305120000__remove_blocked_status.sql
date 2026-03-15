-- Remove the blocked status concept. Tasks no longer enter a blocked state;
-- dependency ordering is handled entirely by the blocker relationship table.

ALTER TABLE tasks DROP COLUMN blocked_from_status;

-- SQLite does not support dropping CHECK constraints inline.
-- The status column check is enforced at the application layer (TaskStatus::parse).
