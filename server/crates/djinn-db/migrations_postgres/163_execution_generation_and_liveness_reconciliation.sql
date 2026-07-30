-- Durable per-task execution fences and task-scoped reconciliation evidence.
ALTER TABLE tasks
    ADD COLUMN execution_generation BIGINT NOT NULL DEFAULT 0;

-- Task-wide reconciliation has no scalar session owner. Nullable foreign-key
-- columns retain their normal FK enforcement whenever a session is supplied.
ALTER TABLE liveness_evidence
    ALTER COLUMN session_id DROP NOT NULL,
    ADD CONSTRAINT liveness_evidence_owner_check
        CHECK (session_id IS NOT NULL OR task_id IS NOT NULL);

-- Reconciliation outcomes supplement, rather than replace, the classifier's
-- existing stable outcome vocabulary.
ALTER TABLE liveness_evidence
    DROP CONSTRAINT liveness_evidence_outcome_kind_check,
    ADD CONSTRAINT liveness_evidence_outcome_kind_check
        CHECK (outcome_kind IS NULL OR outcome_kind IN
            ('success', 'crash', 'timeout', 'dead_reclaimed', 'protocol_violation', 'kill_noop', 'slow_extended',
             'terminated', 'desync_reconciled', 'genuinely_absent', 'task_not_found', 'teardown_failed',
             'settlement_failed', 'reconciliation_incomplete', 'audit_failed'));

-- Keep denormalized session/run snapshots able to represent evidence outcomes
-- when a later repository operation has one unambiguous scalar owner.
ALTER TABLE sessions
    DROP CONSTRAINT sessions_liveness_outcome_kind_check,
    ADD CONSTRAINT sessions_liveness_outcome_kind_check
        CHECK (liveness_outcome_kind IS NULL OR liveness_outcome_kind IN
            ('success', 'crash', 'timeout', 'dead_reclaimed', 'protocol_violation', 'kill_noop', 'slow_extended',
             'terminated', 'desync_reconciled', 'genuinely_absent', 'task_not_found', 'teardown_failed',
             'settlement_failed', 'reconciliation_incomplete', 'audit_failed'));

ALTER TABLE task_runs
    DROP CONSTRAINT task_runs_liveness_outcome_kind_check,
    ADD CONSTRAINT task_runs_liveness_outcome_kind_check
        CHECK (liveness_outcome_kind IS NULL OR liveness_outcome_kind IN
            ('success', 'crash', 'timeout', 'dead_reclaimed', 'protocol_violation', 'kill_noop', 'slow_extended',
             'terminated', 'desync_reconciled', 'genuinely_absent', 'task_not_found', 'teardown_failed',
             'settlement_failed', 'reconciliation_incomplete', 'audit_failed'));
