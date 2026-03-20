-- Add state-tracking columns to tasks for full state machine support.
ALTER TABLE tasks ADD COLUMN blocked_from_status TEXT;
ALTER TABLE tasks ADD COLUMN close_reason TEXT;
