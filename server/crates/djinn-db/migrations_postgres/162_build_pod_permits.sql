-- Durable, conservative admission permits for build-capable task-run Jobs.
--
-- A permit remains active until the controller explicitly releases it. In
-- particular, Job terminality is deliberately not represented here: a terminal
-- Job that remains present keeps its non-released permit and still counts.
CREATE SEQUENCE build_pod_permit_fencing_token_seq AS BIGINT;

CREATE TABLE build_pod_permits (
    -- There is exactly one durable permit lifecycle for each task run.
    task_run_id             VARCHAR(36)  PRIMARY KEY,
    -- Stable permit identity for idempotent retries and audit correlation.
    permit_id               UUID         NOT NULL DEFAULT gen_random_uuid() UNIQUE,
    -- Monotonically allocated ownership fence. It is retained after release so
    -- stale reconcilers cannot mistake a historical permit for a new owner.
    fencing_token           BIGINT       NOT NULL DEFAULT nextval('build_pod_permit_fencing_token_seq') UNIQUE,
    state                   VARCHAR(32)  NOT NULL DEFAULT 'acquired',
    acquired_at             TIMESTAMPTZ  NOT NULL DEFAULT now(),
    -- A Job may be absent while creation is in flight or being reconciled.
    job_uid                 VARCHAR(255) NULL,
    -- These fields are the sole release event. A Job becoming terminal is not.
    released_at             TIMESTAMPTZ  NULL,
    released_fencing_token  BIGINT       NULL,
    release_reason          VARCHAR(64)  NULL,
    CONSTRAINT fk_build_pod_permits_task_run
        FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE RESTRICT,
    CONSTRAINT build_pod_permits_state_check
        CHECK (state IN ('acquired', 'job_created', 'released')),
    CONSTRAINT build_pod_permits_fencing_token_check
        CHECK (fencing_token > 0),
    CONSTRAINT build_pod_permits_job_uid_state_check CHECK (
        (state = 'acquired' AND job_uid IS NULL)
        OR (state = 'job_created' AND job_uid IS NOT NULL)
        OR state = 'released'
    ),
    CONSTRAINT build_pod_permits_release_check CHECK (
        (state = 'released'
            AND released_at IS NOT NULL
            AND released_fencing_token = fencing_token
            AND release_reason IS NOT NULL)
        OR
        (state <> 'released'
            AND released_at IS NULL
            AND released_fencing_token IS NULL
            AND release_reason IS NULL)
    )
);

-- `WHERE state <> 'released'` is the authoritative active-count shape. The
-- partial index keeps terminal-but-present Jobs in the count until a fenced,
-- explicit release changes the permit state.
CREATE INDEX build_pod_permits_active_idx
    ON build_pod_permits (task_run_id)
    WHERE state <> 'released';

-- Permit and fencing identities, and a once-observed Job UID, are immutable.
-- State and explicit release metadata deliberately remain writable through the
-- downstream repository's conditional, fence-matching transitions.
CREATE FUNCTION build_pod_permits_prevent_immutable_mutation() RETURNS trigger AS $$
BEGIN
    IF NEW.task_run_id <> OLD.task_run_id
       OR NEW.permit_id <> OLD.permit_id
       OR NEW.fencing_token <> OLD.fencing_token
       OR (OLD.job_uid IS NOT NULL AND NEW.job_uid IS DISTINCT FROM OLD.job_uid) THEN
        RAISE EXCEPTION 'build pod permit immutable identity, fencing token or Job UID changed';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER build_pod_permits_immutable_trigger BEFORE UPDATE ON build_pod_permits
FOR EACH ROW EXECUTE FUNCTION build_pod_permits_prevent_immutable_mutation();

-- This one row is locked with SELECT ... FOR UPDATE by permit transactions.
-- The literal key prevents creation of an independent pool by accident.
CREATE TABLE build_pod_permit_pools (
    pool_key    VARCHAR(32) NOT NULL PRIMARY KEY CHECK (pool_key = 'global'),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The migration runner records applied migrations, but this is also harmless
-- when its initialization statement is deliberately replayed during upgrades.
INSERT INTO build_pod_permit_pools (pool_key) VALUES ('global')
ON CONFLICT (pool_key) DO NOTHING;
