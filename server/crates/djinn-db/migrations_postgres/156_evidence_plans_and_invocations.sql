-- Migration 156: frozen grounded-evidence plans and immutable command ledger.
-- Additive substrate only. Existing EvidenceFindings debate records remain untouched.

CREATE TABLE IF NOT EXISTS evidence_plans (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    spike_task_id VARCHAR(36) NOT NULL,
    session_id VARCHAR(36) NOT NULL,
    captured_commit_sha VARCHAR(128) NOT NULL,
    worktree_fingerprint VARCHAR(512) NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT evidence_plans_task_session_unique UNIQUE (spike_task_id, session_id),
    CONSTRAINT evidence_plans_identity_unique UNIQUE (id, spike_task_id, session_id, captured_commit_sha, worktree_fingerprint),
    CONSTRAINT evidence_plans_task_fk FOREIGN KEY (spike_task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT evidence_plans_session_fk FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    CONSTRAINT evidence_plans_commit_nonempty CHECK (length(btrim(captured_commit_sha)) > 0),
    CONSTRAINT evidence_plans_worktree_nonempty CHECK (length(btrim(worktree_fingerprint)) > 0)
);

CREATE TABLE IF NOT EXISTS evidence_plan_checks (
    plan_id VARCHAR(36) NOT NULL,
    ordinal INTEGER NOT NULL,
    check_id VARCHAR(128) NOT NULL,
    question TEXT NOT NULL,
    method VARCHAR(32) NOT NULL,
    PRIMARY KEY (plan_id, check_id),
    CONSTRAINT evidence_plan_checks_plan_ordinal_unique UNIQUE (plan_id, ordinal),
    CONSTRAINT evidence_plan_checks_plan_fk FOREIGN KEY (plan_id) REFERENCES evidence_plans(id) ON DELETE CASCADE,
    CONSTRAINT evidence_plan_checks_ordinal_positive CHECK (ordinal > 0),
    CONSTRAINT evidence_plan_checks_id_nonempty CHECK (length(btrim(check_id)) > 0),
    CONSTRAINT evidence_plan_checks_question_nonempty CHECK (length(btrim(question)) > 0),
    CONSTRAINT evidence_plan_checks_method_check CHECK (method IN ('code', 'graph', 'command'))
);

CREATE TABLE IF NOT EXISTS evidence_command_invocations (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    plan_id VARCHAR(36) NOT NULL,
    spike_task_id VARCHAR(36) NOT NULL,
    session_id VARCHAR(36) NOT NULL,
    captured_commit_sha VARCHAR(128) NOT NULL,
    worktree_fingerprint VARCHAR(512) NOT NULL,
    check_id VARCHAR(128) NOT NULL,
    argv JSONB NOT NULL,
    canonical_cwd VARCHAR(2048) NOT NULL,
    launch_state VARCHAR(32) NOT NULL,
    process_state VARCHAR(32) NOT NULL,
    launched_at VARCHAR(64) NULL,
    finished_at VARCHAR(64) NULL,
    exit_code INTEGER NULL,
    signal INTEGER NULL,
    runner_failure TEXT NULL,
    elapsed_millis BIGINT NULL,
    timeout_millis BIGINT NULL,
    timed_out BOOLEAN NOT NULL DEFAULT FALSE,
    stdout_digest VARCHAR(128) NULL,
    stdout_excerpt TEXT NULL,
    stdout_truncated BOOLEAN NOT NULL DEFAULT FALSE,
    stderr_digest VARCHAR(128) NULL,
    stderr_excerpt TEXT NULL,
    stderr_truncated BOOLEAN NOT NULL DEFAULT FALSE,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT evidence_command_invocations_identity_fk FOREIGN KEY
        (plan_id, spike_task_id, session_id, captured_commit_sha, worktree_fingerprint)
        REFERENCES evidence_plans (id, spike_task_id, session_id, captured_commit_sha, worktree_fingerprint),
    CONSTRAINT evidence_command_invocations_check_fk FOREIGN KEY (plan_id, check_id)
        REFERENCES evidence_plan_checks (plan_id, check_id),
    CONSTRAINT evidence_command_invocations_argv_array CHECK (jsonb_typeof(argv) = 'array' AND jsonb_array_length(argv) > 0),
    CONSTRAINT evidence_command_invocations_cwd_nonempty CHECK (length(btrim(canonical_cwd)) > 0),
    CONSTRAINT evidence_command_invocations_launch_state_check CHECK (launch_state IN ('not_started', 'launched', 'failed_to_launch')),
    CONSTRAINT evidence_command_invocations_process_state_check CHECK (process_state IN ('not_started', 'running', 'exited', 'signaled', 'runner_failed', 'timed_out')),
    CONSTRAINT evidence_command_invocations_elapsed_nonnegative CHECK (elapsed_millis IS NULL OR elapsed_millis >= 0),
    CONSTRAINT evidence_command_invocations_timeout_positive CHECK (timeout_millis IS NULL OR timeout_millis > 0)
);

CREATE INDEX IF NOT EXISTS idx_evidence_command_invocations_plan_check_created
    ON evidence_command_invocations(plan_id, check_id, created_at);

CREATE TABLE IF NOT EXISTS evidence_finalized_projections (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    plan_id VARCHAR(36) NOT NULL UNIQUE,
    version INTEGER NOT NULL,
    payload JSONB NOT NULL,
    finalized_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT evidence_finalized_projections_plan_fk FOREIGN KEY (plan_id) REFERENCES evidence_plans(id) ON DELETE CASCADE,
    CONSTRAINT evidence_finalized_projections_version_positive CHECK (version > 0),
    CONSTRAINT evidence_finalized_projections_payload_object CHECK (jsonb_typeof(payload) = 'object')
);

-- A plan is frozen once captured. Invocation rows are a forensic append-only
-- ledger: retrying a command inserts a new opaque id; it can never overwrite
-- the transcript/outcome of a prior invocation.
CREATE OR REPLACE FUNCTION reject_evidence_plan_mutation() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'evidence plans and checks are immutable';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER evidence_plans_immutable BEFORE UPDATE ON evidence_plans
    FOR EACH ROW EXECUTE FUNCTION reject_evidence_plan_mutation();
CREATE TRIGGER evidence_plan_checks_immutable BEFORE UPDATE ON evidence_plan_checks
    FOR EACH ROW EXECUTE FUNCTION reject_evidence_plan_mutation();
CREATE OR REPLACE FUNCTION reject_evidence_invocation_mutation() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'evidence command invocations are append-only';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER evidence_command_invocations_append_only
    BEFORE UPDATE OR DELETE ON evidence_command_invocations
    FOR EACH ROW EXECUTE FUNCTION reject_evidence_invocation_mutation();
