-- Add a merge-queue (`merge_group`) failure lane to the per-task CI snapshot.
--
-- GitHub's merge queue runs the heavy CI stages on the ephemeral `merge_group`
-- ref, not on the PR head. A PR head whose own required checks are green can
-- still be rejected by the queue at dequeue time. The PR poller already
-- discovers those rejections (it looks up the failed `merge_group` Actions run
-- and its failing check runs) but previously only wrote an activity comment —
-- so the durable snapshot said "passing" while the queue kept rejecting, and
-- merge-queue failures never got a failure fingerprint for same-signature
-- counting / escalation.
--
-- These `mq_*` columns form a SECOND lane on the snapshot, written ONLY by the
-- dequeue path and never touched by the PR-head writer (to avoid flip-flop).
-- The PR-head writer clears this lane when it observes a NEW head SHA (a new
-- head invalidates the old queue verdict).
--
-- Purely additive: every column is nullable / defaulted, no backfill.

-- Every column is nullable with NO default: an absent merge-queue observation
-- is represented by NULL, and an explicit `DEFAULT NULL` on `ADD COLUMN` would
-- persist a `NULL::type` default expression (unlike `CREATE TABLE`), so it is
-- omitted deliberately.
ALTER TABLE task_pr_ci_snapshots
    ADD COLUMN IF NOT EXISTS mq_state                TEXT        NULL,
    ADD COLUMN IF NOT EXISTS mq_run_id               BIGINT      NULL,
    ADD COLUMN IF NOT EXISTS mq_head_sha             VARCHAR(64) NULL,
    ADD COLUMN IF NOT EXISTS mq_failed_check_names   JSONB       NULL,
    ADD COLUMN IF NOT EXISTS mq_failure_fingerprint  TEXT        NULL,
    ADD COLUMN IF NOT EXISTS mq_same_signature_count BIGINT      NULL,
    ADD COLUMN IF NOT EXISTS mq_first_seen_at        VARCHAR(64) NULL,
    ADD COLUMN IF NOT EXISTS mq_last_seen_at         VARCHAR(64) NULL;

-- When present, mq_failed_check_names must be a JSON array (mirrors the
-- PR-head lane's blocking_required_check_names invariant), but NULL is allowed
-- (no merge-queue observation for this head).
ALTER TABLE task_pr_ci_snapshots
    ADD CONSTRAINT task_pr_ci_snapshots_mq_failed_names_array_check
        CHECK (mq_failed_check_names IS NULL
               OR jsonb_typeof(mq_failed_check_names) = 'array');

ALTER TABLE task_pr_ci_snapshots
    ADD CONSTRAINT task_pr_ci_snapshots_mq_same_signature_count_check
        CHECK (mq_same_signature_count IS NULL OR mq_same_signature_count >= 0);
