-- 72_drop_verification_tables.sql
--
-- Remove the verification pre-PR gate backing tables now that all code
-- references have been removed (epic sehj: tasks 5glf, 34pj, fspe, npn6).
--
-- 1. Drain in-flight tasks: any task still sitting in the `verifying` status
--    would otherwise be stranded once the verification pipeline (and its
--    tables) are gone. Move them to `needs_task_review` so they resurface in
--    the human review queue instead of hanging forever.
-- 2. Drop the five verification-related tables. CASCADE cleans up dependent
--    objects (indexes, foreign keys) so the order of names in the list does
--    not matter.
--
-- Historical CREATE migrations (44, 45, 62, and the initial schema) remain
-- untouched — they are still applied on fresh databases and this migration
-- drops what they create. Running both is idempotent because every CREATE uses
-- IF NOT EXISTS and every DROP here uses IF EXISTS.

-- 1. Drain in-flight verification tasks to the human review queue.
UPDATE tasks
    SET status = 'needs_task_review'
    WHERE status = 'verifying';

-- 2. Drop all verification-related tables.
DROP TABLE IF EXISTS
    verification_cache,
    verification_results,
    verification_test_runs,
    verification_runs,
    project_verifications
    CASCADE;
