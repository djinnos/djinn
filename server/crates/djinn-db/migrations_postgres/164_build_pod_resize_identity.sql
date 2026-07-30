-- Immutable native-sidecar resize identity, attached to the existing permit.
ALTER TABLE build_pod_permits
    ADD COLUMN pod_namespace VARCHAR(253),
    ADD COLUMN pod_name VARCHAR(253),
    ADD COLUMN pod_uid VARCHAR(255),
    ADD COLUMN launcher_container_name VARCHAR(253),
    ADD COLUMN launcher_container_id VARCHAR(255),
    ADD COLUMN image_digest VARCHAR(512),
    ADD COLUMN observed_launcher_protocol VARCHAR(32),
    ADD COLUMN effective_launcher_protocol VARCHAR(32),
    ADD COLUMN admitted_cpu_millicores BIGINT;

ALTER TABLE build_pod_permits DROP CONSTRAINT build_pod_permits_state_check;
ALTER TABLE build_pod_permits DROP CONSTRAINT build_pod_permits_job_uid_state_check;
ALTER TABLE build_pod_permits
    ADD CONSTRAINT build_pod_permits_state_check CHECK (state IN (
        'acquired', 'job_created', 'birth_confirmed', 'lift_applying', 'lifted',
        'drop_required', 'drop_applying', 'quarantined', 'released'
    )),
    ADD CONSTRAINT build_pod_permits_job_uid_state_check CHECK (
        (state = 'acquired' AND job_uid IS NULL) OR
        (state IN ('job_created', 'birth_confirmed', 'lift_applying', 'lifted',
                   'drop_required', 'drop_applying', 'quarantined') AND job_uid IS NOT NULL)
        -- Migration 162 deliberately permits release before a Job is observed.
        -- Preserve that valid historical shape during this additive upgrade.
        OR state = 'released'
    ),
    ADD CONSTRAINT build_pod_permits_resize_identity_check CHECK (
        (pod_namespace IS NULL AND pod_name IS NULL AND pod_uid IS NULL
         AND launcher_container_name IS NULL AND launcher_container_id IS NULL
         AND image_digest IS NULL AND observed_launcher_protocol IS NULL
         AND effective_launcher_protocol IS NULL AND admitted_cpu_millicores IS NULL)
        OR
        (pod_namespace IS NOT NULL AND pod_name IS NOT NULL AND pod_uid IS NOT NULL
         AND launcher_container_name IS NOT NULL AND launcher_container_id IS NOT NULL
         AND image_digest IS NOT NULL AND observed_launcher_protocol IN ('leaf-v1', 'resize-v2')
         AND effective_launcher_protocol IN ('leaf-v1', 'resize-v2')
         AND admitted_cpu_millicores > 0)
    ),
    ADD CONSTRAINT build_pod_permits_resize_state_identity_check CHECK (
        state NOT IN ('birth_confirmed', 'lift_applying', 'lifted', 'drop_required', 'drop_applying', 'quarantined')
        OR pod_uid IS NOT NULL
    );

-- A Pod belongs to exactly one permit. The per-row write-once predicate alone
-- only stops the *same* permit being recaptured; without this index two task
-- runs could each capture the same Pod UID with different admitted ceilings and
-- both believe they hold write-once authority over that Pod.
CREATE UNIQUE INDEX build_pod_permits_pod_uid_key ON build_pod_permits (pod_uid)
    WHERE pod_uid IS NOT NULL;

CREATE INDEX build_pod_permits_resize_nonterminal_idx ON build_pod_permits (acquired_at, task_run_id)
    WHERE state IN ('birth_confirmed', 'lift_applying', 'lifted', 'drop_required', 'drop_applying', 'quarantined');

CREATE OR REPLACE FUNCTION build_pod_permits_prevent_immutable_mutation() RETURNS trigger AS $$
BEGIN
    IF NEW.task_run_id <> OLD.task_run_id OR NEW.permit_id <> OLD.permit_id
       OR NEW.fencing_token <> OLD.fencing_token
       OR (OLD.job_uid IS NOT NULL AND NEW.job_uid IS DISTINCT FROM OLD.job_uid)
       OR (OLD.pod_uid IS NOT NULL AND (NEW.pod_namespace, NEW.pod_name, NEW.pod_uid,
           NEW.launcher_container_name, NEW.launcher_container_id, NEW.image_digest,
           NEW.observed_launcher_protocol, NEW.effective_launcher_protocol, NEW.admitted_cpu_millicores)
           IS DISTINCT FROM (OLD.pod_namespace, OLD.pod_name, OLD.pod_uid,
           OLD.launcher_container_name, OLD.launcher_container_id, OLD.image_digest,
           OLD.observed_launcher_protocol, OLD.effective_launcher_protocol, OLD.admitted_cpu_millicores)) THEN
        RAISE EXCEPTION 'build pod permit immutable identity or fencing token changed';
    END IF;
    IF NOT ((OLD.state = 'acquired' AND NEW.state IN ('job_created', 'released'))
        OR (OLD.state = 'job_created' AND NEW.state IN ('birth_confirmed', 'released'))
        OR (OLD.state = 'birth_confirmed' AND NEW.state IN ('lift_applying', 'drop_required', 'quarantined', 'released'))
        OR (OLD.state = 'lift_applying' AND NEW.state IN ('lifted', 'drop_required', 'quarantined', 'released'))
        OR (OLD.state = 'lifted' AND NEW.state IN ('drop_required', 'quarantined', 'released'))
        OR (OLD.state = 'drop_required' AND NEW.state IN ('drop_applying', 'quarantined', 'released'))
        OR (OLD.state = 'drop_applying' AND NEW.state IN ('birth_confirmed', 'quarantined', 'released'))
        OR (OLD.state = 'quarantined' AND NEW.state = 'released')
        OR (OLD.state = NEW.state)) THEN
        RAISE EXCEPTION 'illegal build pod resize lifecycle transition';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
