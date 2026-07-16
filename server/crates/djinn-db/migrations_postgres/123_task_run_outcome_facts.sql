-- Migration 123: additive immutable task-run outcome facts.
-- Historical task/session/CI state is intentionally not consulted or backfilled.
ALTER TABLE task_attempts ADD COLUMN IF NOT EXISTS task_run_id VARCHAR(36) NULL;
ALTER TABLE task_attempts ADD CONSTRAINT fk_task_attempts_task_run
    FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE SET NULL;
ALTER TABLE task_attempts ADD CONSTRAINT task_attempts_task_run_id_unique UNIQUE (task_run_id);
CREATE INDEX IF NOT EXISTS idx_task_attempts_task_run_id ON task_attempts (task_run_id)
    WHERE task_run_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS task_run_outcome_facts (
    task_run_id VARCHAR(36) PRIMARY KEY REFERENCES task_runs(id) ON DELETE CASCADE,
    -- Snapshotted only from the explicitly associated task_attempt.
    attempt_seq INTEGER NULL CHECK (attempt_seq > 0),
    outcome VARCHAR(32) NOT NULL DEFAULT 'legacy_unknown'
        CHECK (outcome IN ('legacy_unknown', 'observed')),
    parked_reason VARCHAR(64),
    review_verdict VARCHAR(32) CHECK (review_verdict IS NULL OR review_verdict IN ('accepted', 'rejected', 'not_applicable')),
    merge_queue_result VARCHAR(32) CHECK (merge_queue_result IS NULL OR merge_queue_result IN ('passed', 'failed', 'not_applicable')),
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);
