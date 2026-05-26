-- Migration 25: widen INT (INT4) columns whose Rust counterparts are `i64`
-- to BIGINT (INT8), so sqlx can decode them without a type mismatch.
--
-- Root cause: several columns were created as Postgres `INT` (INT4) but the
-- corresponding `djinn_core` / repo structs type them as Rust `i64` (INT8).
-- Non-macro `sqlx::query_as::<_, T>(...)` decodes then fail at runtime with
--   decoding column "<c>": mismatched types; Rust type i64 (INT8) is not
--   compatible with SQL type INT4
-- which broke the dispatcher (`TaskRepository::list_ready` + `claim`) and the
-- Kanban board (`list_filtered`). The macro `query_as!(...)` variants over the
-- same columns only compiled against a stale `.sqlx` cache and would fail at
-- runtime identically.
--
-- Aligning the column to BIGINT matches the i64 Rust types and fixes the
-- macro + non-macro queries uniformly (preferred over per-query `::int8`
-- casts or downgrading the structs to i32).
--
-- Columns deliberately LEFT as INT (their Rust field is i32 / decoded as
-- i32): org_config.id, note_embeddings.embedding_dim,
-- note_embedding_meta.embedding_dim, verification_results.step_index,
-- verification_results.exit_code, code_chunks.start_line, code_chunks.end_line.

-- ── tasks (all 7 counters are i64 in djinn_core::models::task::Task) ─────────
ALTER TABLE tasks ALTER COLUMN priority                         TYPE BIGINT;
ALTER TABLE tasks ALTER COLUMN reopen_count                     TYPE BIGINT;
ALTER TABLE tasks ALTER COLUMN continuation_count               TYPE BIGINT;
ALTER TABLE tasks ALTER COLUMN verification_failure_count       TYPE BIGINT;
ALTER TABLE tasks ALTER COLUMN total_reopen_count               TYPE BIGINT;
ALTER TABLE tasks ALTER COLUMN total_verification_failure_count TYPE BIGINT;
ALTER TABLE tasks ALTER COLUMN intervention_count               TYPE BIGINT;

-- ── consolidation_run_metrics (ConsolidationRunMetric fields are i64) ────────
ALTER TABLE consolidation_run_metrics ALTER COLUMN scanned_note_count         TYPE BIGINT;
ALTER TABLE consolidation_run_metrics ALTER COLUMN candidate_cluster_count    TYPE BIGINT;
ALTER TABLE consolidation_run_metrics ALTER COLUMN consolidated_cluster_count TYPE BIGINT;
ALTER TABLE consolidation_run_metrics ALTER COLUMN consolidated_note_count    TYPE BIGINT;
ALTER TABLE consolidation_run_metrics ALTER COLUMN source_note_count          TYPE BIGINT;
