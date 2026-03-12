-- Make sessions.task_id nullable so project-scoped agents (e.g. groomer)
-- can create sessions without a real task row in the tasks table.

DROP TABLE IF EXISTS sessions_new;
CREATE TABLE sessions_new (
    id               TEXT NOT NULL PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id          TEXT REFERENCES tasks(id) ON DELETE CASCADE,
    model_id         TEXT NOT NULL,
    agent_type       TEXT NOT NULL,
    started_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at         TEXT,
    status           TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed', 'paused', 'compacted')),
    tokens_in        INTEGER NOT NULL DEFAULT 0,
    tokens_out       INTEGER NOT NULL DEFAULT 0,
    worktree_path    TEXT,
    goose_session_id TEXT
);

INSERT INTO sessions_new
    SELECT id, project_id, task_id, model_id, agent_type,
           started_at, ended_at, status, tokens_in, tokens_out,
           worktree_path, goose_session_id
    FROM sessions;

DROP TABLE sessions;

ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX sessions_project_id ON sessions(project_id);
CREATE INDEX sessions_task_id    ON sessions(task_id);
CREATE INDEX sessions_status     ON sessions(status);
