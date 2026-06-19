-- 69_lifecycle_sweep_metric_counts.sql
--
-- Persist lifecycle sweep counters alongside consolidation run metrics. Organic
-- consolidation runs keep the default zero values; housekeeping/operator
-- lifecycle sweeps write note_type = 'lifecycle_sweep' rows with these counts.
ALTER TABLE consolidation_run_metrics
    ADD COLUMN IF NOT EXISTS decayed_note_count BIGINT NOT NULL DEFAULT 0;

ALTER TABLE consolidation_run_metrics
    ADD COLUMN IF NOT EXISTS archived_note_count BIGINT NOT NULL DEFAULT 0;
