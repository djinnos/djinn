-- Migration 17: fix `DEFAULT ""` on timestamp columns.
--
-- `session_messages.created_at` and `consolidated_note_provenance.created_at`
-- were declared `VARCHAR(64) NOT NULL DEFAULT ""` in migration 1, and the
-- corresponding INSERTs in djinn-db never set the column explicitly. Every
-- row therefore stored an empty string. The chat sidebar groups sessions by
-- the latest `session_messages.created_at`; the empty-string parses to 0 ms,
-- which renders as "Dec 31, 1969" in any UTC- timezone.
--
-- Fix: change the default to match the rest of the schema (the same NOW(3)
-- ISO-8601 expression used on `sessions.started_at`, `credentials.created_at`,
-- etc.) and backfill existing empty-string rows from the parent session's
-- `started_at`, which is the closest sensible approximation we have.

UPDATE session_messages sm
JOIN sessions s ON sm.session_id = s.id
SET sm.created_at = s.started_at
WHERE sm.created_at = '';

ALTER TABLE session_messages
    MODIFY COLUMN created_at VARCHAR(64) NOT NULL
        DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'));

UPDATE consolidated_note_provenance cnp
JOIN sessions s ON cnp.session_id = s.id
SET cnp.created_at = s.started_at
WHERE cnp.created_at = '';

ALTER TABLE consolidated_note_provenance
    MODIFY COLUMN created_at VARCHAR(64) NOT NULL
        DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'));
