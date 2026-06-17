-- 63_consolidation_superseded_source_count.sql
--
-- yk9t task dm4w: superseded-source counter for the consolidation run metric.
--
-- When a canonical consolidated note is created, its source notes are marked
-- superseded via typed `note_associations.kind = 'supersedes'` edges. This
-- column records how many source notes were superseded in the run, so sweep
-- metrics / memory_health can surface the count alongside the other lifecycle
-- counters. Default 0 preserves existing rows and legacy callers that do not
-- record supersession edges.
ALTER TABLE consolidation_run_metrics
    ADD COLUMN IF NOT EXISTS superseded_source_note_count BIGINT NOT NULL DEFAULT 0;
