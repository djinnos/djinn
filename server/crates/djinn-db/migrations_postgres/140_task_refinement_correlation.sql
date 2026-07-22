-- Exact task-to-refinement identity. These remain nullable for ordinary and
-- historical tasks. Migration 138 intentionally backfills some historical
-- source tasks with only refinement_run_id, so raw storage must permit that
-- rollout state. The shared model rejects every partial non-null tuple before
-- exposing it as typed correlation.
ALTER TABLE tasks
    ADD COLUMN refinement_generation BIGINT NULL,
    ADD COLUMN refinement_round BIGINT NULL,
    ADD COLUMN refinement_phase VARCHAR(64) NULL,
    ADD COLUMN refinement_role VARCHAR(64) NULL;
