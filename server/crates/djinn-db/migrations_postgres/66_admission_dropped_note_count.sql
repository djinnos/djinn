-- Add admission_dropped_note_count to consolidation_run_metrics.
--
-- Tracks how many candidate extracted notes were rejected by the ADR-054
-- underspecified admission gate during an extraction run.  The field is
-- always NOT NULL DEFAULT 0 so it is never NULL at write time.

ALTER TABLE consolidation_run_metrics
    ADD COLUMN IF NOT EXISTS admission_dropped_note_count BIGINT NOT NULL DEFAULT 0;
