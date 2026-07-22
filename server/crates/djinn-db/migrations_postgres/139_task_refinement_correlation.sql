-- Exact task-to-refinement identity. These remain nullable for ordinary and
-- historical tasks; the shared model rejects every partial non-null tuple.
ALTER TABLE tasks
    ADD COLUMN refinement_generation BIGINT NULL,
    ADD COLUMN refinement_round BIGINT NULL,
    ADD COLUMN refinement_phase VARCHAR(64) NULL,
    ADD COLUMN refinement_role VARCHAR(64) NULL,
    ADD CONSTRAINT tasks_refinement_correlation_check CHECK (
        (refinement_run_id IS NULL
         AND refinement_intent_id IS NULL
         AND refinement_generation IS NULL
         AND refinement_round IS NULL
         AND refinement_phase IS NULL
         AND refinement_role IS NULL)
        OR
        (refinement_run_id IS NOT NULL
         AND refinement_intent_id IS NOT NULL
         AND refinement_generation > 0
         AND refinement_round > 0
         AND refinement_phase IN ('adversary_attack', 'advocate_revision', 'judge_adjudication')
         AND refinement_role IN ('adversary', 'advocate', 'judge')
         AND ((refinement_phase = 'adversary_attack' AND refinement_role = 'adversary')
           OR (refinement_phase = 'advocate_revision' AND refinement_role = 'advocate')
           OR (refinement_phase = 'judge_adjudication' AND refinement_role = 'judge')))
    );
