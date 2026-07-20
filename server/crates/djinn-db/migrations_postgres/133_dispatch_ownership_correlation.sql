CREATE TABLE IF NOT EXISTS coordinator_incarnations (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    registered_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    last_renewed_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);
ALTER TABLE task_attempts ADD COLUMN IF NOT EXISTS dispatch_owner_incarnation_id VARCHAR(36) NULL, ADD COLUMN IF NOT EXISTS dispatch_group_id VARCHAR(36) NULL;
ALTER TABLE task_runs ADD COLUMN IF NOT EXISTS dispatch_group_id VARCHAR(36) NULL;
CREATE INDEX IF NOT EXISTS idx_task_attempts_dispatch_owner_incarnation ON task_attempts(dispatch_owner_incarnation_id) WHERE dispatch_owner_incarnation_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_task_attempts_dispatch_group ON task_attempts(dispatch_group_id) WHERE dispatch_group_id IS NOT NULL AND outcome IN ('pending', 'submitted');
CREATE INDEX IF NOT EXISTS idx_task_runs_dispatch_group ON task_runs(dispatch_group_id) WHERE dispatch_group_id IS NOT NULL;
