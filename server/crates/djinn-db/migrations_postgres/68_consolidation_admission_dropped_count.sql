ALTER TABLE consolidation_run_metrics
    ADD COLUMN IF NOT EXISTS admission_dropped_note_count BIGINT NOT NULL DEFAULT 0;
