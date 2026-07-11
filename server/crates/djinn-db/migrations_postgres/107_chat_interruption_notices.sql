-- Durable, model-internal markers for assistant turns interrupted while streaming.
-- Kept separate from session_messages so notices never become visible content.
CREATE TABLE IF NOT EXISTS chat_interruption_notices (
    interruption_notice_id      VARCHAR(36) PRIMARY KEY,
    session_id                  VARCHAR(36) NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    session_message_id          VARCHAR(36) REFERENCES session_messages(id) ON DELETE SET NULL,
    interrupted_turn            BOOLEAN NOT NULL DEFAULT true,
    discarded_tool_calls_count  INTEGER NOT NULL DEFAULT 0 CHECK (discarded_tool_calls_count >= 0),
    interrupted_at              VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    consumed_at                 VARCHAR(64)
);

CREATE INDEX IF NOT EXISTS chat_interruption_notices_unconsumed_session_idx
    ON chat_interruption_notices (session_id, interrupted_at)
    WHERE consumed_at IS NULL;
