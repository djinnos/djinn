-- Extend learned_prompt_history.action to allow 'confirmed' (post-evaluation keep).
-- SQLite does not support ALTER COLUMN, so we rebuild the table.
-- Previous values: 'keep' (proposed, pending evaluation), 'discard' (reverted).
-- New value: 'confirmed' (evaluated post N tasks, metrics improved or neutral).

CREATE TABLE learned_prompt_history_new (
    id              TEXT NOT NULL PRIMARY KEY,
    role_id         TEXT NOT NULL REFERENCES agent_roles(id) ON DELETE CASCADE,
    proposed_text   TEXT NOT NULL,
    action          TEXT NOT NULL CHECK(action IN ('keep', 'discard', 'confirmed')),
    metrics_before  TEXT,
    metrics_after   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO learned_prompt_history_new
    SELECT id, role_id, proposed_text, action, metrics_before, metrics_after, created_at
    FROM learned_prompt_history;

DROP TABLE learned_prompt_history;

ALTER TABLE learned_prompt_history_new RENAME TO learned_prompt_history;

CREATE INDEX learned_prompt_history_role_id ON learned_prompt_history(role_id);
