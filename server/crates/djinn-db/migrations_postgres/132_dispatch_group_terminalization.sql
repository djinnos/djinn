-- Migration 132: exact dispatch-group terminalization substrate (epic jy7g).
-- Legacy NULL-group rows remain deliberately uncorrelated.
ALTER TABLE task_attempts
    ADD COLUMN IF NOT EXISTS dispatch_group_id VARCHAR(36) NULL;

CREATE INDEX IF NOT EXISTS idx_task_attempts_dispatch_group_pending
    ON task_attempts(dispatch_group_id)
    WHERE dispatch_group_id IS NOT NULL AND outcome = 'pending';
