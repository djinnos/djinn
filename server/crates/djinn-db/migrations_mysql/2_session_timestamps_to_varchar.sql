-- user_auth_sessions.created_at/expires_at were DATETIME(3), which sqlx
-- decodes as chrono/time types — but the repository reads them as String
-- (matching every other timestamp column in the schema, which are all
-- VARCHAR(64) holding RFC3339). Rewrite to VARCHAR(64) for consistency.

ALTER TABLE user_auth_sessions
    MODIFY COLUMN created_at VARCHAR(64) NOT NULL
        DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'));

ALTER TABLE user_auth_sessions
    MODIFY COLUMN expires_at VARCHAR(64) NOT NULL;
