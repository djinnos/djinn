-- Persist agent session lifecycle + token accounting per task.

CREATE TABLE sessions (
    id            TEXT NOT NULL PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    model_id      TEXT NOT NULL,
    agent_type    TEXT NOT NULL,
    started_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at      TEXT,
    status        TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed')),
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    worktree_path TEXT
);

CREATE INDEX sessions_task_id ON sessions(task_id);
CREATE INDEX sessions_status ON sessions(status);
