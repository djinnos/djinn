-- Foundation for session cost accounting: nullable `cost_usd` and start-time
-- per-million price snapshots. Later tasks populate these from the model
-- catalog at session creation and recompute `cost_usd` from snapshot rates
-- whenever token counts are written. Unpriced/uncatalogued sessions remain
-- NULL (never treated as $0/free).
--
-- All new columns are nullable with NO default, so existing rows and sessions
-- created before pricing logic is wired up stay NULL.
ALTER TABLE sessions
    ADD COLUMN cost_usd DOUBLE PRECISION NULL,
    ADD COLUMN input_price_per_million_snapshot DOUBLE PRECISION NULL,
    ADD COLUMN output_price_per_million_snapshot DOUBLE PRECISION NULL,
    ADD COLUMN cache_read_price_per_million_snapshot DOUBLE PRECISION NULL,
    ADD COLUMN cache_write_price_per_million_snapshot DOUBLE PRECISION NULL;

-- Analytics-supporting indexes for range/grouped scans. These supplement the
-- existing `idx_sessions_started_at`, `idx_sessions_project_id`,
-- `idx_sessions_status`, and `idx_sessions_task_id` indexes from the initial
-- schema (none are removed). Each composite starts with `started_at` (the
-- primary range-scan column for time-windowed usage analytics) and then carries
-- common grouping dimensions so time-bounded usage reports can group/filter by
-- project, model, and session creator without depending on a single-column
-- timestamp index alone.
CREATE INDEX idx_sessions_started_at_project_id_model_id
    ON sessions(started_at, project_id, model_id);
CREATE INDEX idx_sessions_started_at_model_id
    ON sessions(started_at, model_id);
CREATE INDEX idx_sessions_started_at_created_by_user_id_model_id
    ON sessions(started_at, created_by_user_id, model_id);
