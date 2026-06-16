-- 61_verification_runs.sql
--
-- Backing store for the pre-PR verification pipeline when it executes in a
-- one-shot Kubernetes Job (the production path). Verification commands need the
-- project's toolchain + the shared /cache PVC, neither of which exist on the
-- djinn-server host — running them inline there exits 127 ("cargo: not found")
-- and false-fails every task. Instead the server dispatches a Job in the
-- project image that runs the real verification pipeline (`verify_commit`)
-- against the task branch's tree and writes its outcome here; the server polls
-- this row for the result, then runs the existing pass/fail transitions.
--
-- Mirrors `verification_test_runs` (migration 45) but is keyed on a concrete
-- task rather than a candidate rule set. Non-macro `sqlx::query` form so a fresh
-- table doesn't require regenerating the offline `.sqlx` cache.

CREATE TABLE IF NOT EXISTS verification_runs (
    id           VARCHAR(36)  NOT NULL PRIMARY KEY,
    task_id      VARCHAR(36)  NOT NULL,
    project_id   VARCHAR(36)  NOT NULL,
    -- pending | running | passed | failed | error
    status       VARCHAR(32)  NOT NULL DEFAULT 'pending',
    -- Setup-phase per-command results:
    -- [{name, command, exit_code, stdout, stderr, duration_ms}].
    setup_results        JSONB NOT NULL DEFAULT '[]'::jsonb,
    -- Verification-phase per-command results (same shape).
    verification_results JSONB NOT NULL DEFAULT '[]'::jsonb,
    error        TEXT         NULL,
    created_at   VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    completed_at VARCHAR(64)  NULL,
    CONSTRAINT fk_verification_runs_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX verification_runs_task ON verification_runs(task_id);
CREATE INDEX verification_runs_project ON verification_runs(project_id);
CREATE INDEX verification_runs_status ON verification_runs(status);
