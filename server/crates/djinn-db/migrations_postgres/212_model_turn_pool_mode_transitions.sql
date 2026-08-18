-- The durable ledger of per-pool admission-*mode* changes.
--
-- `model_turn_pools.phase` had no production writer at all: the only writers
-- were test fixtures. This table is the audit half of the first production
-- writer, so "the pool went to draining before that acquisition could commit"
-- and "a drain on an already-off pool did nothing" are both countable facts
-- rather than log lines.
--
-- It stores no credential id, account id, project id, user id, request id, or
-- lease id: a mode change is (pool route, from, to, closed-vocabulary reason,
-- controller generation, instant).
CREATE TABLE model_turn_pool_mode_transitions (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    from_mode VARCHAR(16) NOT NULL,
    to_mode VARCHAR(16) NOT NULL,
    reason VARCHAR(32) NOT NULL,
    controller_generation BIGINT NOT NULL,
    -- `clock_timestamp()`, not `now()`: this row is written after the pool row
    -- was taken FOR UPDATE, so the instant must be the real one at that point
    -- rather than the transaction's start. A concurrent acquisition that won
    -- the lock therefore always carries an earlier `reserved_at` than this.
    changed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT model_turn_pool_mode_transitions_from_valid
        CHECK (from_mode IN ('off', 'shadow', 'draining', 'enforce')),
    CONSTRAINT model_turn_pool_mode_transitions_to_valid
        CHECK (to_mode IN ('off', 'shadow', 'draining', 'enforce')),
    CONSTRAINT model_turn_pool_mode_transitions_moves
        CHECK (from_mode <> to_mode),
    CONSTRAINT model_turn_pool_mode_transitions_reason_valid CHECK (reason IN (
        'operator_request',
        'capability_coverage_loss',
        'identity_ineligible',
        'rollback',
        'drain_settled',
        'enforcement_advance'
    )),
    CONSTRAINT model_turn_pool_mode_transitions_generation_positive
        CHECK (controller_generation > 0)
);
CREATE INDEX model_turn_pool_mode_transitions_pool_recent_idx
    ON model_turn_pool_mode_transitions (pool_id, changed_at DESC, id DESC);
