-- wnqw: CI triage signal — ranked primary blocking check, captured annotations,
-- and the `inconclusive` CI status.
--
-- Before this migration the durable CI snapshot recorded only check NAMES plus
-- a failure fingerprint. When a run-level cancel (fail-fast watcher, runner-host
-- crash) swept every in-flight sibling to `cancelled`, consumers derived the
-- "primary blocking check" as the alphabetically first blocking name — which
-- could be a `needs:`-dependent aggregator that never executed and therefore
-- cannot be a root cause. The real diagnosis lived in a GitHub *annotation* that
-- was never captured at all.
--
-- Three additive changes:
--   1. `primary_blocking_check` — the ranked triage target, computed from
--      structural execution evidence rather than name order. NULL when no
--      blocking check carries causal information.
--   2. `failure_annotations` — bounded rendering of the annotations on that
--      check. Runner-host failures surface only here.
--   3. `ci_status` gains `inconclusive` — a completed run in which every
--      blocking required check was cancelled or never executed reached no
--      verdict about the code, and warrants a retrigger rather than a
--      remediation attempt.

ALTER TABLE task_pr_ci_snapshots
    ADD COLUMN IF NOT EXISTS primary_blocking_check TEXT,
    ADD COLUMN IF NOT EXISTS failure_annotations    TEXT;

ALTER TABLE task_pr_ci_snapshots
    DROP CONSTRAINT IF EXISTS task_pr_ci_snapshots_ci_status_check;

ALTER TABLE task_pr_ci_snapshots
    ADD CONSTRAINT task_pr_ci_snapshots_ci_status_check
        CHECK (ci_status IN ('passing', 'failing', 'pending', 'inconclusive', 'unknown'));

-- Backfill the new column for rows written before this migration. The old
-- derivation ("first blocking name") is preserved only for rows that are still
-- `failing`, so existing board state does not go blank on deploy; the poller
-- overwrites it with the ranked value on the next observation of each PR head.
UPDATE task_pr_ci_snapshots
   SET primary_blocking_check = blocking_required_check_names ->> 0
 WHERE primary_blocking_check IS NULL
   AND ci_status = 'failing'
   AND jsonb_typeof(blocking_required_check_names) = 'array'
   AND jsonb_array_length(blocking_required_check_names) > 0;
