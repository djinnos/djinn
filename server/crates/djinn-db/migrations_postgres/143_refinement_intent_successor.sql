-- Bind a completed phase to the one durable successor it selected. This makes
-- non-identical completion retries rejectable rather than additional work.
ALTER TABLE refinement_dispatch_intents
    ADD COLUMN next_intent_id VARCHAR(36) NULL
        REFERENCES refinement_dispatch_intents(id) ON DELETE RESTRICT;

CREATE UNIQUE INDEX refinement_dispatch_intents_next_intent_unique
    ON refinement_dispatch_intents(next_intent_id)
    WHERE next_intent_id IS NOT NULL;
