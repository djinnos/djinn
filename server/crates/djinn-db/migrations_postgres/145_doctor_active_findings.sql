ALTER TABLE doctor_findings ADD COLUMN IF NOT EXISTS active_key VARCHAR(1024) NULL;
ALTER TABLE doctor_findings ADD COLUMN IF NOT EXISTS status VARCHAR(16) NOT NULL DEFAULT 'active';
ALTER TABLE doctor_findings ADD COLUMN IF NOT EXISTS observed_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"');
ALTER TABLE doctor_findings DROP CONSTRAINT IF EXISTS doctor_findings_severity_check;
ALTER TABLE doctor_findings ADD CONSTRAINT doctor_findings_severity_check CHECK (severity IN ('info','warn','critical','error'));
ALTER TABLE doctor_findings ADD CONSTRAINT doctor_findings_status_check CHECK (status IN ('active','resolved'));
CREATE UNIQUE INDEX IF NOT EXISTS doctor_findings_active_key_idx ON doctor_findings (check_name,active_key) WHERE active_key IS NOT NULL;
