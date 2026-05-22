-- Migration 17: fix `DEFAULT ''` on timestamp columns.
--
-- `session_messages.created_at` and `consolidated_note_provenance.created_at`
-- were declared `VARCHAR(64) NOT NULL DEFAULT ''` in migration 1, and the
-- corresponding INSERTs in djinn-db never set the column explicitly. Every
-- row therefore stored an empty string. The chat sidebar groups sessions by
-- the latest `session_messages.created_at`; the empty-string parses to 0 ms,
-- which renders as "Dec 31, 1969" in any UTC- timezone.

UPDATE session_messages sm
SET created_at = s.started_at
FROM sessions s
WHERE sm.session_id = s.id AND sm.created_at = '';

ALTER TABLE session_messages
    ALTER COLUMN created_at SET DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"');

UPDATE consolidated_note_provenance cnp
SET created_at = s.started_at
FROM sessions s
WHERE cnp.session_id = s.id AND cnp.created_at = '';

ALTER TABLE consolidated_note_provenance
    ALTER COLUMN created_at SET DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"');
