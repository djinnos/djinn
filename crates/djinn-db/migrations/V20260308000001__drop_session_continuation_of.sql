DROP INDEX IF EXISTS idx_sessions_continuation_of;
ALTER TABLE sessions DROP COLUMN continuation_of;
