CREATE UNIQUE INDEX tasks_refinement_intent_id_unique
    ON tasks(refinement_intent_id) WHERE refinement_intent_id IS NOT NULL;
