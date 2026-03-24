-- Lifetime counters: monotonically increasing, never reset.
ALTER TABLE tasks ADD COLUMN total_reopen_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN total_verification_failure_count INTEGER NOT NULL DEFAULT 0;

-- Intervention tracking.
ALTER TABLE tasks ADD COLUMN intervention_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN last_intervention_at TEXT;

-- Backfill from current counters (may undercount due to prior resets).
UPDATE tasks SET
    total_reopen_count = reopen_count,
    total_verification_failure_count = verification_failure_count;
