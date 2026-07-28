-- Migration 158: fenced deterministic readiness aggregation.
ALTER TABLE readiness_guardrail_findings
    ADD COLUMN status VARCHAR(32) NOT NULL DEFAULT 'covered',
    ADD CONSTRAINT readiness_findings_status_check
        CHECK (status IN ('covered','partial','missing','unknown','unsupported','analysis_error'));

CREATE TABLE readiness_area_scores (
    run_id VARCHAR(36) NOT NULL REFERENCES readiness_runs(id) ON DELETE CASCADE,
    area_id VARCHAR(36) NOT NULL REFERENCES readiness_composition_areas(id) ON DELETE CASCADE,
    score DOUBLE PRECISION NOT NULL,
    applicable_weight INTEGER NOT NULL,
    covered_weight DOUBLE PRECISION NOT NULL,
    status VARCHAR(32) NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (run_id, area_id),
    CONSTRAINT readiness_area_scores_status_check CHECK (status IN ('supported','unsupported')),
    CONSTRAINT readiness_area_scores_weight_check CHECK (applicable_weight >= 0 AND covered_weight >= 0)
);
CREATE TABLE readiness_project_scores (
    run_id VARCHAR(36) PRIMARY KEY REFERENCES readiness_runs(id) ON DELETE CASCADE,
    score DOUBLE PRECISION NOT NULL,
    band VARCHAR(16) NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT readiness_project_scores_band_check CHECK (band IN ('blocked','emerging','ready','strong'))
);
ALTER TABLE readiness_runs ADD COLUMN aggregation_owner VARCHAR(255) NULL;
ALTER TABLE readiness_runs ADD COLUMN aggregation_generation INTEGER NOT NULL DEFAULT 0;

CREATE TRIGGER readiness_area_scores_terminal_guard
    BEFORE INSERT OR UPDATE OR DELETE ON readiness_area_scores
    FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
CREATE TRIGGER readiness_project_scores_terminal_guard
    BEFORE INSERT OR UPDATE OR DELETE ON readiness_project_scores
    FOR EACH ROW EXECUTE FUNCTION readiness_reject_completed_child_write();
