-- Persisted verification step results so the frontend can load them on page open
-- instead of relying on transient SSE events.
CREATE TABLE IF NOT EXISTS verification_results (
    id          TEXT    NOT NULL PRIMARY KEY DEFAULT (lower(hex(randomblob(16)))),
    project_id  TEXT    NOT NULL,
    task_id     TEXT,
    run_id      TEXT    NOT NULL,
    phase       TEXT    NOT NULL CHECK (phase IN ('setup', 'verification')),
    step_index  INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    command     TEXT    NOT NULL DEFAULT '',
    exit_code   INTEGER NOT NULL,
    stdout      TEXT    NOT NULL DEFAULT '',
    stderr      TEXT    NOT NULL DEFAULT '',
    duration_ms INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_verification_results_task
    ON verification_results (task_id, created_at DESC);

CREATE INDEX idx_verification_results_project
    ON verification_results (project_id, created_at DESC);

CREATE INDEX idx_verification_results_run
    ON verification_results (run_id);
