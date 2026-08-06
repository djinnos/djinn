-- Dispatch errors are immutable audit facts. The retry reservation survives
-- them so a coordinator can re-drive the same task identity.
CREATE TABLE typed_evidence_retry_dispatch_errors (
    id VARCHAR(36) PRIMARY KEY,
    finding_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_findings(id) ON DELETE RESTRICT,
    attempt_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_attempts(id) ON DELETE RESTRICT,
    spike_task_id VARCHAR(36) NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
    error TEXT NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_retry_dispatch_error_nonempty CHECK (length(btrim(error)) > 0)
);
CREATE OR REPLACE FUNCTION reject_typed_evidence_retry_dispatch_error_mutation() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'typed evidence retry dispatch errors are append-only';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER typed_evidence_retry_dispatch_errors_append_only
    BEFORE UPDATE OR DELETE ON typed_evidence_retry_dispatch_errors
    FOR EACH ROW EXECUTE FUNCTION reject_typed_evidence_retry_dispatch_error_mutation();
