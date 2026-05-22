-- user_auth_sessions.created_at/expires_at were TIMESTAMP(3), which sqlx
-- decodes as chrono/time types — but the repository reads them as String
-- (matching every other timestamp column in the schema, which are all
-- VARCHAR(64) holding RFC3339). Rewrite to VARCHAR(64) for consistency.

ALTER TABLE user_auth_sessions
    ALTER COLUMN created_at DROP DEFAULT,
    ALTER COLUMN created_at TYPE VARCHAR(64) USING to_char(created_at AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    ALTER COLUMN created_at SET DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"');

ALTER TABLE user_auth_sessions
    ALTER COLUMN expires_at TYPE VARCHAR(64) USING to_char(expires_at AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"');
