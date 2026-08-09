-- dser / lnfd C0: dormant, additive storage for attempt-scoped delivery.
-- Existing task-PR columns and lifecycle remain authoritative while the epoch
-- is disabled; nullable direct-delivery fields are never an activation signal.

-- A task with no proposal owner remains legacy. A task with both an epic owner
-- and a different breakdown-task owner is unsafe to classify, so report every
-- such open task and abort before any direct-delivery relation is created.
DO $$
DECLARE ambiguous_tasks TEXT;
BEGIN
    SELECT string_agg(format('%s (%s)', id, short_id), ', ' ORDER BY id)
      INTO ambiguous_tasks
      FROM (
          SELECT t.id, t.short_id
            FROM tasks t
            LEFT JOIN epics e ON e.id = t.epic_id
           CROSS JOIN LATERAL (
                SELECT e.proposal_id AS proposal_id WHERE e.proposal_id IS NOT NULL
                UNION
                SELECT p.id FROM proposals p WHERE p.build_breakdown_task_id = t.id
           ) owners
           WHERE t.status <> 'closed'
           GROUP BY t.id, t.short_id
          HAVING count(*) > 1
      ) ambiguous;
    IF ambiguous_tasks IS NOT NULL THEN
        RAISE EXCEPTION
            'direct_delivery_v1 migration cannot classify ambiguous open task owner(s): %',
            ambiguous_tasks;
    END IF;
END $$;

CREATE TABLE proposal_build_attempts (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    proposal_id VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE RESTRICT,
    short_id VARCHAR(32) NOT NULL,
    lifecycle VARCHAR(16) NOT NULL,
    base_sha VARCHAR(64) NOT NULL,
    branch_head_sha VARCHAR(64) NULL,
    branch_name VARCHAR(512) NOT NULL,
    proposal_pr_number BIGINT NULL,
    proposal_pr_url VARCHAR(1024) NULL,
    park_reason VARCHAR(64) NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    activated_at TIMESTAMPTZ NULL,
    retired_at TIMESTAMPTZ NULL,
    CONSTRAINT proposal_build_attempts_lifecycle_check CHECK (lifecycle IN ('reserved', 'active', 'retired')),
    CONSTRAINT proposal_build_attempts_base_sha_nonblank CHECK (btrim(base_sha) <> ''),
    CONSTRAINT proposal_build_attempts_branch_name_nonblank CHECK (btrim(branch_name) <> ''),
    CONSTRAINT proposal_build_attempts_park_reason_check CHECK (park_reason IS NULL OR park_reason IN ('branch_identity_mismatch', 'proposal_pr_identity_mismatch', 'unexpected_branch_head', 'delivery_conflict', 'no_proposal_owner', 'capability_unavailable', 'epoch_disabled', 'lease_lost')),
    CONSTRAINT proposal_build_attempts_branch_identity_unique UNIQUE (branch_name),
    CONSTRAINT proposal_build_attempts_pr_identity_unique UNIQUE (proposal_pr_number)
);
CREATE UNIQUE INDEX proposal_build_attempts_one_active_per_proposal ON proposal_build_attempts (proposal_id) WHERE lifecycle = 'active';
CREATE UNIQUE INDEX proposal_build_attempts_proposal_short_id_unique ON proposal_build_attempts (proposal_id, short_id);

CREATE TABLE direct_delivery_epochs (
    name VARCHAR(64) NOT NULL PRIMARY KEY,
    state VARCHAR(16) NOT NULL,
    generation BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT direct_delivery_epochs_name_check CHECK (name = 'direct_delivery_v1'),
    CONSTRAINT direct_delivery_epochs_state_check CHECK (state IN ('disabled', 'active')),
    CONSTRAINT direct_delivery_epochs_generation_check CHECK (generation >= 0)
);
INSERT INTO direct_delivery_epochs (name, state, generation) VALUES ('direct_delivery_v1', 'disabled', 0);

CREATE TABLE direct_delivery_process_capabilities (
    process_incarnation_id VARCHAR(128) NOT NULL,
    capability VARCHAR(32) NOT NULL,
    epoch_generation BIGINT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (process_incarnation_id, capability),
    CONSTRAINT direct_delivery_capability_nonblank CHECK (btrim(process_incarnation_id) <> ''),
    CONSTRAINT direct_delivery_capability_check CHECK (capability IN ('schema', 'provider', 'repository', 'orchestrator', 'consumer_cutover')),
    CONSTRAINT direct_delivery_capability_generation_check CHECK (epoch_generation >= 0)
);
CREATE INDEX direct_delivery_process_capabilities_generation ON direct_delivery_process_capabilities (epoch_generation, observed_at);

CREATE TABLE task_deliveries (
    build_attempt_id VARCHAR(36) NOT NULL REFERENCES proposal_build_attempts(id) ON DELETE RESTRICT,
    task_id VARCHAR(36) NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
    delivery_generation BIGINT NOT NULL,
    state VARCHAR(16) NOT NULL,
    candidate_sha VARCHAR(64) NOT NULL,
    base_sha VARCHAR(64) NOT NULL,
    applied_at TIMESTAMPTZ NULL,
    conflict_reason TEXT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (build_attempt_id, task_id, delivery_generation),
    CONSTRAINT task_deliveries_generation_positive CHECK (delivery_generation > 0),
    CONSTRAINT task_deliveries_state_check CHECK (state IN ('prepared', 'applying', 'applied', 'conflict')),
    CONSTRAINT task_deliveries_candidate_nonblank CHECK (btrim(candidate_sha) <> ''),
    CONSTRAINT task_deliveries_base_nonblank CHECK (btrim(base_sha) <> ''),
    CONSTRAINT task_deliveries_applied_shape CHECK ((state = 'applied' AND applied_at IS NOT NULL AND conflict_reason IS NULL) OR (state = 'conflict' AND applied_at IS NULL AND conflict_reason IS NOT NULL) OR (state IN ('prepared', 'applying') AND applied_at IS NULL AND conflict_reason IS NULL))
);
CREATE UNIQUE INDEX task_deliveries_one_non_conflict_generation ON task_deliveries (build_attempt_id, task_id) WHERE state <> 'conflict';

CREATE FUNCTION prevent_task_delivery_identity_rewrite() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.build_attempt_id IS DISTINCT FROM NEW.build_attempt_id
       OR OLD.task_id IS DISTINCT FROM NEW.task_id
       OR OLD.delivery_generation IS DISTINCT FROM NEW.delivery_generation
       OR OLD.candidate_sha IS DISTINCT FROM NEW.candidate_sha
       OR OLD.base_sha IS DISTINCT FROM NEW.base_sha
       OR OLD.created_at IS DISTINCT FROM NEW.created_at THEN
        RAISE EXCEPTION 'task delivery generation identity is immutable';
    END IF;
    IF OLD.state IN ('applied', 'conflict') THEN
        RAISE EXCEPTION 'terminal task delivery generation is immutable';
    END IF;
    IF (OLD.state = 'prepared' AND NEW.state NOT IN ('prepared', 'applying', 'conflict'))
       OR (OLD.state = 'applying' AND NEW.state NOT IN ('applying', 'applied', 'conflict')) THEN
        RAISE EXCEPTION 'illegal task delivery state transition from % to %', OLD.state, NEW.state;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER task_deliveries_immutable_generation
    BEFORE UPDATE ON task_deliveries
    FOR EACH ROW EXECUTE FUNCTION prevent_task_delivery_identity_rewrite();

CREATE TABLE direct_delivery_leases (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    build_attempt_id VARCHAR(36) NOT NULL,
    task_id VARCHAR(36) NOT NULL,
    delivery_generation BIGINT NOT NULL,
    owner_incarnation_id VARCHAR(128) NOT NULL,
    epoch_generation BIGINT NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    released_at TIMESTAMPTZ NULL,
    CONSTRAINT direct_delivery_leases_delivery_fk FOREIGN KEY (build_attempt_id, task_id, delivery_generation) REFERENCES task_deliveries(build_attempt_id, task_id, delivery_generation) ON DELETE RESTRICT,
    CONSTRAINT direct_delivery_leases_owner_nonblank CHECK (btrim(owner_incarnation_id) <> ''),
    CONSTRAINT direct_delivery_leases_epoch_generation_check CHECK (epoch_generation >= 0),
    CONSTRAINT direct_delivery_leases_expiry_check CHECK (expires_at > acquired_at)
);
CREATE UNIQUE INDEX direct_delivery_leases_one_live_generation ON direct_delivery_leases (build_attempt_id, task_id, delivery_generation) WHERE released_at IS NULL;

COMMENT ON TABLE direct_delivery_epochs IS 'Persisted direct_delivery_v1 activation fence. Default disabled; nullable direct rows never activate writers.';
