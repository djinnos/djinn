-- Preserve clean generations that lose an expected-old ref update to a ledger-mapped head.
ALTER TABLE task_deliveries ADD COLUMN supersede_transition_id VARCHAR(128);
ALTER TABLE task_deliveries
    DROP CONSTRAINT task_deliveries_state_check,
    DROP CONSTRAINT task_deliveries_applied_shape,
    ADD CONSTRAINT task_deliveries_state_check CHECK (state IN ('prepared', 'applying', 'applied', 'conflict', 'superseded')),
    ADD CONSTRAINT task_deliveries_applied_shape CHECK (
        (state = 'applied' AND applied_at IS NOT NULL AND conflict_reason IS NULL AND supersede_transition_id IS NULL)
        OR (state = 'conflict' AND applied_at IS NULL AND conflict_reason IS NOT NULL AND supersede_transition_id IS NULL)
        OR (state = 'superseded' AND applied_at IS NULL AND conflict_reason IS NULL AND supersede_transition_id IS NOT NULL)
        OR (state IN ('prepared', 'applying') AND applied_at IS NULL AND conflict_reason IS NULL AND supersede_transition_id IS NULL)
    );
DROP INDEX task_deliveries_one_non_conflict_generation;
CREATE UNIQUE INDEX task_deliveries_one_live_generation ON task_deliveries (build_attempt_id, task_id) WHERE state IN ('prepared', 'applying');
CREATE UNIQUE INDEX task_deliveries_supersede_transition_unique ON task_deliveries (build_attempt_id, task_id, supersede_transition_id) WHERE supersede_transition_id IS NOT NULL;
CREATE OR REPLACE FUNCTION prevent_task_delivery_identity_rewrite() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.build_attempt_id IS DISTINCT FROM NEW.build_attempt_id OR OLD.task_id IS DISTINCT FROM NEW.task_id OR OLD.delivery_generation IS DISTINCT FROM NEW.delivery_generation OR OLD.candidate_sha IS DISTINCT FROM NEW.candidate_sha OR OLD.base_sha IS DISTINCT FROM NEW.base_sha OR OLD.source_sha IS DISTINCT FROM NEW.source_sha OR OLD.patch_digest IS DISTINCT FROM NEW.patch_digest OR OLD.selected_parent_sha IS DISTINCT FROM NEW.selected_parent_sha OR OLD.prepare_transition_id IS DISTINCT FROM NEW.prepare_transition_id OR OLD.created_at IS DISTINCT FROM NEW.created_at THEN RAISE EXCEPTION 'task delivery generation identity is immutable'; END IF;
    IF OLD.state IN ('applied', 'conflict', 'superseded') THEN RAISE EXCEPTION 'terminal task delivery generation is immutable'; END IF;
    IF (OLD.state = 'prepared' AND NEW.state NOT IN ('prepared', 'applying', 'conflict', 'superseded')) OR (OLD.state = 'applying' AND NEW.state NOT IN ('applying', 'applied', 'conflict', 'superseded')) THEN RAISE EXCEPTION 'illegal task delivery state transition from % to %', OLD.state, NEW.state; END IF;
    RETURN NEW;
END $$;
