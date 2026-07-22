-- v1 durable build-lease ledger.  This is intentionally independent from the
-- v0 admission_journal: rows are retained for audit and stable identity replay.
CREATE SEQUENCE build_lease_fencing_token_seq AS BIGINT;

CREATE TABLE build_lease_caps (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    cap BIGINT NOT NULL CHECK (cap >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO build_lease_caps (singleton, cap) VALUES (TRUE, 0);

CREATE TABLE build_leases (
    consumer_kind VARCHAR(32) NOT NULL,
    consumer_id VARCHAR(255) NOT NULL,
    immutable_identity TEXT NOT NULL,
    enqueue_sequence BIGSERIAL NOT NULL UNIQUE,
    fencing_token BIGINT NULL UNIQUE,
    state VARCHAR(32) NOT NULL,
    queue_deadline TIMESTAMPTZ NULL,
    launch_deadline TIMESTAMPTZ NULL,
    bound_pod_uid VARCHAR(255) NULL,
    candidate_cleanup JSONB NULL,
    terminal_reason VARCHAR(64) NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    granted_at TIMESTAMPTZ NULL,
    terminal_at TIMESTAMPTZ NULL,
    PRIMARY KEY (consumer_kind, consumer_id),
    CONSTRAINT build_leases_consumer_kind_check CHECK (consumer_kind IN ('task_invocation', 'graph_warm')),
    CONSTRAINT build_leases_state_check CHECK (state IN ('queued', 'granted', 'launching', 'bound', 'active', 'suspect', 'terminal')),
    CONSTRAINT build_leases_fencing_check CHECK ((state IN ('granted', 'launching', 'bound', 'active', 'suspect') AND fencing_token IS NOT NULL) OR state IN ('queued', 'terminal')),
    CONSTRAINT build_leases_terminal_check CHECK ((state = 'terminal' AND terminal_at IS NOT NULL) OR (state <> 'terminal' AND terminal_at IS NULL)),
    CONSTRAINT build_leases_bound_uid_check CHECK (bound_pod_uid IS NULL OR state IN ('bound', 'active', 'suspect', 'terminal'))
);

CREATE INDEX build_leases_fifo_queued_idx ON build_leases (enqueue_sequence) WHERE state = 'queued';
CREATE INDEX build_leases_occupied_idx ON build_leases (state) WHERE state IN ('granted', 'launching', 'bound', 'active', 'suspect');

-- Identity fields and pod UID are write-once. State/deadline/cleanup evidence
-- remain mutable only through the repository's explicitly fenced transitions.
CREATE FUNCTION build_leases_prevent_immutable_mutation() RETURNS trigger AS $$
BEGIN
    IF NEW.consumer_kind <> OLD.consumer_kind OR NEW.consumer_id <> OLD.consumer_id
       OR NEW.immutable_identity <> OLD.immutable_identity
       OR (OLD.bound_pod_uid IS NOT NULL AND NEW.bound_pod_uid IS DISTINCT FROM OLD.bound_pod_uid) THEN
        RAISE EXCEPTION 'build lease immutable identity or pod UID changed';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER build_leases_immutable_trigger BEFORE UPDATE ON build_leases
FOR EACH ROW EXECUTE FUNCTION build_leases_prevent_immutable_mutation();
