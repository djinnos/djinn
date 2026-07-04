-- uv3p Part C: consumed-hold marker.
--
-- When a human closes a `human-review-hold` remediation task, the source task
-- it was holding is released back to `open`. Before this marker the park
-- evaluator re-fired immediately on the still-unchanged strike/intervention
-- counters and spawned a duplicate hold within ~150ms (the ygj0 incident).
--
-- `human_review_resolved_at` records the UTC instant the hold was resolved.
-- The strike accounting the park guard reads (`quality_reopen_count`) ignores
-- reopen evidence that predates this marker, so only NEW post-release strikes
-- can re-park the task. Same ISO-8601 text format as the other timestamp
-- columns (`last_intervention_at`, `activity_log.created_at`) so lexical
-- comparison is correct.
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS human_review_resolved_at VARCHAR(64);
