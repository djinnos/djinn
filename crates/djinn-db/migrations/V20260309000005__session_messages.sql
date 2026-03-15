-- Conversation message storage for agent sessions.
-- Replaces Goose's separate sessions.db per ADR-027.

CREATE TABLE session_messages (
    id           TEXT NOT NULL PRIMARY KEY,
    session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role         TEXT NOT NULL CHECK(role IN ('system', 'user', 'assistant')),
    content_json TEXT NOT NULL,
    token_count  INTEGER,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX session_messages_session_id ON session_messages(session_id);
