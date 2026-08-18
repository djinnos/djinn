-- Phase D compatibility-phase record and its transactional guard ledger.
--
-- `model_turn_pools.phase` is the per-pool admission *mode*
-- (`off|shadow|draining|enforce`). It is deliberately NOT the deployment
-- compatibility phase. This migration adds the missing A→B→C→D record as a
-- separate column plus an append-only decision ledger, so a phase can only
-- become effective after every prerequisite was evaluated in one transaction.
--
-- The ledger stores no credential id, account id, project id, user id, request
-- id, or lease id: a decision is (pool route, requested phase, predicate
-- booleans, controller generation) and nothing else.
ALTER TABLE model_turn_pools
    ADD COLUMN compatibility_phase VARCHAR(1) NOT NULL DEFAULT 'a';
ALTER TABLE model_turn_pools
    ADD CONSTRAINT model_turn_pools_compatibility_phase_valid
    CHECK (compatibility_phase IN ('a', 'b', 'c', 'd'));

CREATE TABLE model_turn_pool_phase_transitions (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    requested_phase VARCHAR(1) NOT NULL,
    effective_phase VARCHAR(1) NOT NULL,
    decided_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    predicate_results JSONB NOT NULL,
    controller_generation BIGINT NOT NULL,
    CONSTRAINT model_turn_pool_phase_transitions_requested_valid
        CHECK (requested_phase IN ('a', 'b', 'c', 'd')),
    CONSTRAINT model_turn_pool_phase_transitions_effective_valid
        CHECK (effective_phase IN ('a', 'b', 'c', 'd')),
    CONSTRAINT model_turn_pool_phase_transitions_generation_positive
        CHECK (controller_generation > 0),
    -- Closed shape. The key set must equal the allow-list exactly: `?&`
    -- rejects a missing key and the `- text[]` difference rejects an extra
    -- one, so an unknown key cannot be stored even by a downlevel writer.
    -- Every value must be a boolean, which leaves no room for a free-text
    -- diagnostic to carry an identifier into the ledger.
    CONSTRAINT model_turn_pool_phase_transitions_predicates_closed CHECK (
        jsonb_typeof(predicate_results) = 'object'
        AND predicate_results ?& ARRAY[
            'schema_marker',
            'capability_reports',
            'leadership_generation',
            'observation_history',
            'expected_path_coverage',
            'identity_eligibility'
        ]
        AND predicate_results - ARRAY[
            'schema_marker',
            'capability_reports',
            'leadership_generation',
            'observation_history',
            'expected_path_coverage',
            'identity_eligibility'
        ] = '{}'::jsonb
        AND jsonb_typeof(predicate_results -> 'schema_marker') = 'boolean'
        AND jsonb_typeof(predicate_results -> 'capability_reports') = 'boolean'
        AND jsonb_typeof(predicate_results -> 'leadership_generation') = 'boolean'
        AND jsonb_typeof(predicate_results -> 'observation_history') = 'boolean'
        AND jsonb_typeof(predicate_results -> 'expected_path_coverage') = 'boolean'
        AND jsonb_typeof(predicate_results -> 'identity_eligibility') = 'boolean'
    )
);
CREATE INDEX model_turn_pool_phase_transitions_pool_recent_idx
    ON model_turn_pool_phase_transitions (pool_id, decided_at DESC, id DESC);
