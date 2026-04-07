-- Add event_taxonomy column to sessions for structural session extraction.
-- Stores a JSON blob with: files_changed, errors, git_ops, tools_used,
-- notes_read, notes_written, tasks_transitioned.
ALTER TABLE sessions ADD COLUMN event_taxonomy TEXT;
