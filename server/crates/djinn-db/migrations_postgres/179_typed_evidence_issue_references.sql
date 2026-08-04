-- Keep V1 normative issue codes losslessly and retain the planned-check
-- relationship instead of encoding it into free-form issue detail.
ALTER TABLE typed_evidence_issues
    ALTER COLUMN code TYPE TEXT,
    ADD COLUMN planned_check_id VARCHAR(36)
        REFERENCES typed_evidence_planned_checks(id) ON DELETE RESTRICT;

-- This remains nullable for rows produced by earlier additive versions. The
-- V1 repository always supplies the frozen planned-check reference on insert.
