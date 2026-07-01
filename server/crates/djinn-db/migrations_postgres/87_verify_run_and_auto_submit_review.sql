-- Migration 87: Canonical verify run metadata and auto-submit review substrate.
--
-- Adds two additive tables that later tasks consume for auto-submit decisions:
--
-- 1. `verify_runs` — records the canonical verify execution identity for a
--    task_run: source, external run ID, command/profile versions, completed-at
--    timestamp, result, exact diff fingerprint, and task-specific check coverage.
--
-- 2. `auto_submit_reviews` — records the metadata written when an auto-submit
--    path submits work: trigger reason, diff fingerprint, verify linkage,
--    session/model IDs, no-progress streak, and whether the model called
--    `submit_work`.
--
-- Both tables are additive; no existing objects are dropped or renamed.

-- ─── verify_runs ───────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS verify_runs (
    id              VARCHAR(36)   NOT NULL PRIMARY KEY,
    task_run_id     VARCHAR(36)   NOT NULL,
    verify_source   VARCHAR(64)   NOT NULL,
    verify_run_id   VARCHAR(255)  NOT NULL,
    command_version VARCHAR(128)  NULL,
    profile_version VARCHAR(128)  NULL,
    completed_at    VARCHAR(64)   NOT NULL,
    result          VARCHAR(32)   NOT NULL,
    diff_fingerprint VARCHAR(128) NOT NULL,
    check_coverage  JSONB         NULL,
    created_at      VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_verify_runs_task_run
        FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE CASCADE
);

CREATE INDEX idx_verify_runs_task_run_id ON verify_runs(task_run_id);
CREATE INDEX idx_verify_runs_task_run_id_created_at
    ON verify_runs(task_run_id, created_at DESC);

-- ─── auto_submit_reviews ───────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS auto_submit_reviews (
    id                      VARCHAR(36)   NOT NULL PRIMARY KEY,
    task_run_id             VARCHAR(36)   NOT NULL,
    trigger_reason          VARCHAR(64)   NOT NULL,
    diff_fingerprint        VARCHAR(128)  NOT NULL,
    verify_source           VARCHAR(64)   NULL,
    verify_run_id           VARCHAR(255)  NULL,
    verify_timestamp        VARCHAR(64)   NULL,
    session_id              VARCHAR(36)   NULL,
    model_id                VARCHAR(255)  NULL,
    no_progress_streak      INTEGER       NOT NULL DEFAULT 0,
    model_called_submit_work BOOLEAN      NOT NULL DEFAULT FALSE,
    created_at              VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_auto_submit_reviews_task_run
        FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE CASCADE
);

CREATE INDEX idx_auto_submit_reviews_task_run_id ON auto_submit_reviews(task_run_id);
