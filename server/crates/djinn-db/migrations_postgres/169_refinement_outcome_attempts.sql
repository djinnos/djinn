-- Outcome application can happen after the role task has left the in-memory
-- running pool. Keep its retry budget on the durable intent so restarts cannot
-- reset a permanently failing handoff to zero.
ALTER TABLE refinement_dispatch_intents
    ADD COLUMN outcome_attempts INT NOT NULL DEFAULT 0,
    ADD CONSTRAINT refinement_dispatch_intents_outcome_attempts_nonnegative
        CHECK (outcome_attempts >= 0);

CREATE INDEX idx_refinement_dispatch_intents_stalled_handoff
    ON refinement_dispatch_intents(state, task_id)
    WHERE state = 'materialized' AND task_id IS NOT NULL;
