CREATE TABLE readiness_area_result_outputs (
 run_id VARCHAR(36) NOT NULL REFERENCES readiness_runs(id) ON DELETE CASCADE,
 area_id VARCHAR(36) NOT NULL, attempt_id VARCHAR(36) NOT NULL, result JSONB NOT NULL,
 created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
 PRIMARY KEY (attempt_id),
 FOREIGN KEY (attempt_id, run_id, area_id) REFERENCES readiness_area_attempts(id, run_id, area_id) ON DELETE CASCADE
);
CREATE TRIGGER readiness_result_outputs_terminal_guard BEFORE INSERT OR UPDATE OR DELETE ON readiness_area_result_outputs FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
