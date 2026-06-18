-- 68_drop_verification_tables.sql
--
-- Drop verification pre-PR gate tables.
-- Historical CREATE migrations (1 for `verification_cache` / `verification_results`,
-- 44 for `project_verifications`, 45 for `verification_test_runs`, 62 for
-- `verification_runs`) are preserved as applied history — sqlx records a
-- per-file checksum in `_sqlx_migrations` and refuses to start if any applied
-- file is later mutated, so the CREATE rows stay untouched.
--
-- The verification pre-PR gate has been removed from the codebase (epic u6t5):
-- the worker now completes straight to `needs_task_review` and no task can
-- enter a `Verifying` state.

-- 1. Drain any in-flight tasks still parked in the removed 'verifying' status.
--    No rows should be stranded in a state the rest of the system can no
--    longer transition out of once these tables are gone.
UPDATE tasks SET status = 'needs_task_review'
WHERE status = 'verifying';

-- 2. Drop verification tables. All FKs point at `projects(id)` (no
--    inter-verification-table FKs), so the DROP order is not load-bearing —
--    `IF EXISTS` keeps the migration idempotent if it is re-applied.
DROP TABLE IF EXISTS verification_results;
DROP TABLE IF EXISTS verification_cache;
DROP TABLE IF EXISTS verification_test_runs;
DROP TABLE IF EXISTS verification_runs;
DROP TABLE IF EXISTS project_verifications;
