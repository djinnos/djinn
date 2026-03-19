-- Add learned_prompt to agent_roles for auto-improvement loop.
-- Kept separate from user-written system_prompt_extensions (never auto-modified).

ALTER TABLE agent_roles ADD COLUMN learned_prompt TEXT;

-- Audit trail: every keep/discard decision by the Architect improvement loop.
CREATE TABLE IF NOT EXISTS learned_prompt_history (
    id              TEXT NOT NULL PRIMARY KEY,
    role_id         TEXT NOT NULL REFERENCES agent_roles(id) ON DELETE CASCADE,
    proposed_text   TEXT NOT NULL,
    action          TEXT NOT NULL CHECK(action IN ('keep', 'discard')),
    metrics_before  TEXT,
    metrics_after   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX learned_prompt_history_role_id ON learned_prompt_history(role_id);
