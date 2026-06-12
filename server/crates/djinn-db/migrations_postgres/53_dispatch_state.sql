-- Proposal 8ipw / storage-primitives epic n6xw: durable dispatch state.
--
-- Persists the coordinator's restart-sensitive dispatch-decision state using
-- wall-clock TIMESTAMPTZ values instead of serializing process-local Instants:
--   * task_id: one durable state row per task; cascades when the task is deleted.
--   * failure_streak: consecutive dispatch failures used by the backoff ladder.
--   * cooldown_until: nullable wall-clock deadline before the task is dispatchable.
--   * escalation_count: number of planner/escalation interventions requested.
--   * last_dispatched_at / last_dispatched_role: nullable audit marker for the
--     most recent dispatch attempt.
--   * updated_at: wall-clock timestamp for repository writes and stale-row audits.
--
-- Index choices: idx_dispatch_state_cooldown_until lets the integration epic's
-- periodic cleanup/due scan find cooldown rows efficiently; idx_dispatch_state_
-- escalation_count supports operator/audit queries for repeatedly escalated
-- tasks without a full-table scan. The task_id primary key already provides the
-- natural-key lookup path used by load/upsert/clear operations.

CREATE TABLE IF NOT EXISTS dispatch_state (
    task_id                VARCHAR(36) NOT NULL PRIMARY KEY,
    failure_streak         INT         NOT NULL DEFAULT 0,
    cooldown_until         TIMESTAMPTZ NULL,
    escalation_count       INT         NOT NULL DEFAULT 0,
    last_dispatched_at     TIMESTAMPTZ NULL,
    last_dispatched_role   VARCHAR(64) NULL,
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')::TIMESTAMPTZ),
    CONSTRAINT fk_dispatch_state_task
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);

CREATE INDEX idx_dispatch_state_cooldown_until ON dispatch_state(cooldown_until);
CREATE INDEX idx_dispatch_state_escalation_count ON dispatch_state(escalation_count);
