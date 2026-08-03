-- Migration 176: additive normalized tribunal evidence lifecycle substrate.
--
-- Legacy `proposals.linked_spike_task_id` and `proposals.needs_evidence_claim`
-- remain migration-82 compatibility authority during mixed-version rollout.
-- Frozen plans and command provenance remain in migration 156's evidence_* tables;
-- this schema references them instead of copying their records.

CREATE TABLE typed_evidence_findings (
    id VARCHAR(36) PRIMARY KEY,
    proposal_id VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE RESTRICT,
    demand_hash VARCHAR(128) NOT NULL,
    lifecycle VARCHAR(32) NOT NULL,
    claim JSONB NOT NULL,
    demanded_revision_seq INTEGER NOT NULL,
    created_by_task_id VARCHAR(36) NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_findings_hash_nonempty CHECK (length(btrim(demand_hash)) > 0),
    CONSTRAINT typed_evidence_findings_claim_object CHECK (jsonb_typeof(claim) = 'object'),
    CONSTRAINT typed_evidence_findings_revision_positive CHECK (demanded_revision_seq > 0),
    CONSTRAINT typed_evidence_findings_lifecycle CHECK (lifecycle IN
        ('demanded', 'spike_active', 'evidence_received', 'failed', 'resolved', 'withdrawn'))
);
CREATE UNIQUE INDEX typed_evidence_one_unresolved_finding_per_proposal
    ON typed_evidence_findings(proposal_id)
    WHERE lifecycle IN ('demanded', 'spike_active', 'evidence_received', 'failed');
CREATE UNIQUE INDEX typed_evidence_finding_demand_hash
    ON typed_evidence_findings(proposal_id, demand_hash);

CREATE TABLE typed_evidence_attempts (
    id VARCHAR(36) PRIMARY KEY,
    finding_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_findings(id) ON DELETE RESTRICT,
    sequence INTEGER NOT NULL,
    spike_task_id VARCHAR(36) NOT NULL UNIQUE REFERENCES tasks(id) ON DELETE RESTRICT,
    evidence_plan_id VARCHAR(36) NULL UNIQUE REFERENCES evidence_plans(id) ON DELETE RESTRICT,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_attempt_sequence_positive CHECK (sequence > 0),
    CONSTRAINT typed_evidence_attempt_sequence_unique UNIQUE (finding_id, sequence)
);

CREATE TABLE typed_evidence_transitions (
    id VARCHAR(36) PRIMARY KEY,
    finding_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_findings(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL,
    from_lifecycle VARCHAR(32) NULL,
    to_lifecycle VARCHAR(32) NOT NULL,
    actor_task_id VARCHAR(36) NULL REFERENCES tasks(id) ON DELETE RESTRICT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_transition_ordinal_positive CHECK (ordinal > 0),
    CONSTRAINT typed_evidence_transition_ordinal_unique UNIQUE (finding_id, ordinal),
    CONSTRAINT typed_evidence_transition_from_lifecycle CHECK (from_lifecycle IS NULL OR from_lifecycle IN
        ('demanded', 'spike_active', 'evidence_received', 'failed', 'resolved', 'withdrawn')),
    CONSTRAINT typed_evidence_transition_to_lifecycle CHECK (to_lifecycle IN
        ('demanded', 'spike_active', 'evidence_received', 'failed', 'resolved', 'withdrawn')),
    CONSTRAINT typed_evidence_transition_metadata_object CHECK (jsonb_typeof(metadata) = 'object')
);

CREATE TABLE typed_evidence_planned_checks (
    id VARCHAR(36) PRIMARY KEY,
    attempt_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_attempts(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL,
    check_id VARCHAR(128) NOT NULL,
    method VARCHAR(32) NOT NULL,
    evidence_plan_id VARCHAR(36) NULL,
    evidence_plan_check_id VARCHAR(128) NULL,
    CONSTRAINT typed_evidence_planned_checks_attempt_ordinal_unique UNIQUE (attempt_id, ordinal),
    CONSTRAINT typed_evidence_planned_checks_attempt_check_unique UNIQUE (attempt_id, check_id),
    CONSTRAINT typed_evidence_planned_checks_ordinal_positive CHECK (ordinal > 0),
    CONSTRAINT typed_evidence_planned_checks_id_nonempty CHECK (length(btrim(check_id)) > 0),
    CONSTRAINT typed_evidence_planned_checks_method CHECK (method IN ('code', 'graph', 'command')),
    CONSTRAINT typed_evidence_planned_checks_plan_pair CHECK
        ((evidence_plan_id IS NULL) = (evidence_plan_check_id IS NULL)),
    CONSTRAINT typed_evidence_planned_checks_plan_check_fk FOREIGN KEY (evidence_plan_id, evidence_plan_check_id)
        REFERENCES evidence_plan_checks(plan_id, check_id) ON DELETE RESTRICT
);

CREATE TABLE typed_evidence_validation_results (
    id VARCHAR(36) PRIMARY KEY,
    attempt_id VARCHAR(36) NOT NULL UNIQUE REFERENCES typed_evidence_attempts(id) ON DELETE RESTRICT,
    payload_sha256 VARCHAR(128) NOT NULL,
    outcome VARCHAR(16) NOT NULL,
    validator_facts JSONB NOT NULL,
    validated_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_validation_hash_nonempty CHECK (length(btrim(payload_sha256)) > 0),
    CONSTRAINT typed_evidence_validation_outcome CHECK (outcome IN ('resolved', 'partial', 'unresolved')),
    CONSTRAINT typed_evidence_validation_facts_object CHECK (jsonb_typeof(validator_facts) = 'object')
);

CREATE TABLE typed_evidence_check_results (
    id VARCHAR(36) PRIMARY KEY,
    validation_result_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_validation_results(id) ON DELETE RESTRICT,
    planned_check_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_planned_checks(id) ON DELETE RESTRICT,
    status VARCHAR(16) NOT NULL,
    detail TEXT NULL,
    CONSTRAINT typed_evidence_check_result_unique UNIQUE (validation_result_id, planned_check_id),
    CONSTRAINT typed_evidence_check_result_status CHECK (status IN ('passed', 'failed', 'not_run'))
);

CREATE TABLE typed_evidence_anchors (
    id VARCHAR(36) PRIMARY KEY,
    check_result_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_check_results(id) ON DELETE RESTRICT,
    method VARCHAR(32) NOT NULL,
    locator TEXT NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_anchor_method CHECK (method IN
        ('code', 'graph', 'command', 'artifact', 'memory', 'external', 'repository')),
    CONSTRAINT typed_evidence_anchor_locator_nonempty CHECK (length(btrim(locator)) > 0)
);
CREATE TABLE typed_evidence_anchor_health (
    anchor_id VARCHAR(36) PRIMARY KEY REFERENCES typed_evidence_anchors(id) ON DELETE RESTRICT,
    health VARCHAR(16) NOT NULL,
    detail TEXT NULL,
    observed_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_anchor_health_status CHECK (health IN ('healthy', 'unusable', 'unavailable'))
);

CREATE TABLE typed_evidence_issues (
    id VARCHAR(36) PRIMARY KEY,
    validation_result_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_validation_results(id) ON DELETE RESTRICT,
    kind VARCHAR(16) NOT NULL,
    code VARCHAR(128) NOT NULL,
    detail TEXT NOT NULL,
    CONSTRAINT typed_evidence_issue_kind CHECK (kind IN ('failure', 'gap')),
    CONSTRAINT typed_evidence_issue_code_nonempty CHECK (length(btrim(code)) > 0),
    CONSTRAINT typed_evidence_issue_detail_nonempty CHECK (length(btrim(detail)) > 0)
);

CREATE TABLE typed_evidence_dispositions (
    id VARCHAR(36) PRIMARY KEY,
    finding_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_findings(id) ON DELETE RESTRICT,
    validation_result_id VARCHAR(36) NULL REFERENCES typed_evidence_validation_results(id) ON DELETE RESTRICT,
    folding_revision INTEGER NOT NULL,
    outcome VARCHAR(16) NOT NULL,
    disposition VARCHAR(16) NOT NULL,
    judge_task_id VARCHAR(36) NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
    rationale TEXT NOT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT typed_evidence_disposition_finding_revision_unique UNIQUE (finding_id, folding_revision),
    CONSTRAINT typed_evidence_disposition_revision_positive CHECK (folding_revision > 0),
    CONSTRAINT typed_evidence_disposition_outcome CHECK (outcome IN ('resolved', 'partial', 'unresolved')),
    CONSTRAINT typed_evidence_disposition_terminal CHECK (disposition IN ('resolved', 'withdrawn')),
    CONSTRAINT typed_evidence_disposition_rationale_nonempty CHECK (length(btrim(rationale)) > 0)
);

CREATE TABLE typed_evidence_retry_idempotency (
    finding_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_findings(id) ON DELETE RESTRICT,
    failed_transition_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_transitions(id) ON DELETE RESTRICT,
    retry_attempt_id VARCHAR(36) NOT NULL UNIQUE REFERENCES typed_evidence_attempts(id) ON DELETE RESTRICT,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (finding_id, failed_transition_id)
);

-- Lifecycle history is forensic evidence. Transition rows cannot be rewritten
-- or deleted even by direct SQL; later repositories append a new transition.
CREATE OR REPLACE FUNCTION reject_typed_evidence_transition_mutation() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'typed evidence transitions are append-only';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER typed_evidence_transitions_append_only
    BEFORE UPDATE OR DELETE ON typed_evidence_transitions
    FOR EACH ROW EXECUTE FUNCTION reject_typed_evidence_transition_mutation();
