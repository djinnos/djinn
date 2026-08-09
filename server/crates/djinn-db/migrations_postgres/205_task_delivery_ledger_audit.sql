-- C1: immutable preparation facts and auditable idempotency identities.
ALTER TABLE task_deliveries
    ADD COLUMN source_sha VARCHAR(64) NOT NULL DEFAULT '',
    ADD COLUMN patch_digest VARCHAR(128) NOT NULL DEFAULT '',
    ADD COLUMN selected_parent_sha VARCHAR(64) NOT NULL DEFAULT '',
    ADD COLUMN prepare_transition_id VARCHAR(128) NOT NULL DEFAULT '',
    ADD COLUMN applying_transition_id VARCHAR(128),
    ADD COLUMN finalization_transition_id VARCHAR(128);
ALTER TABLE task_deliveries
    ADD CONSTRAINT task_deliveries_source_nonblank CHECK (btrim(source_sha) <> ''),
    ADD CONSTRAINT task_deliveries_patch_digest_nonblank CHECK (btrim(patch_digest) <> ''),
    ADD CONSTRAINT task_deliveries_parent_nonblank CHECK (btrim(selected_parent_sha) <> ''),
    ADD CONSTRAINT task_deliveries_prepare_transition_nonblank CHECK (btrim(prepare_transition_id) <> '');
CREATE UNIQUE INDEX task_deliveries_prepare_transition_unique
    ON task_deliveries (build_attempt_id, task_id, prepare_transition_id);
CREATE UNIQUE INDEX task_deliveries_finalization_transition_unique
    ON task_deliveries (build_attempt_id, task_id, finalization_transition_id)
    WHERE finalization_transition_id IS NOT NULL;

CREATE OR REPLACE FUNCTION prevent_task_delivery_identity_rewrite() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.build_attempt_id IS DISTINCT FROM NEW.build_attempt_id
       OR OLD.task_id IS DISTINCT FROM NEW.task_id
       OR OLD.delivery_generation IS DISTINCT FROM NEW.delivery_generation
       OR OLD.candidate_sha IS DISTINCT FROM NEW.candidate_sha
       OR OLD.base_sha IS DISTINCT FROM NEW.base_sha
       OR OLD.source_sha IS DISTINCT FROM NEW.source_sha
       OR OLD.patch_digest IS DISTINCT FROM NEW.patch_digest
       OR OLD.selected_parent_sha IS DISTINCT FROM NEW.selected_parent_sha
       OR OLD.prepare_transition_id IS DISTINCT FROM NEW.prepare_transition_id
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
