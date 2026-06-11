-- Persist per-session prompt-cache token totals.
--
-- Previously cache_read/cache_write counts only flowed to OTel/Langfuse
-- telemetry (ADR-043), which is unconfigured in most deployments, so cache
-- hit-rate was invisible from the DB. These running totals are written
-- alongside `tokens_in`/`tokens_out` on session finalization.
ALTER TABLE sessions
    ADD COLUMN cache_read_tokens bigint NOT NULL DEFAULT 0,
    ADD COLUMN cache_write_tokens bigint NOT NULL DEFAULT 0;
