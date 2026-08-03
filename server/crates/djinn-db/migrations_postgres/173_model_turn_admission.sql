-- Additive, inert v1 storage for per-credential model-turn admission. Pool
-- identity deliberately stops at the durable credentials row plus provider/model
-- scope: no owner-user identity or credential material is copied into this ledger.
CREATE TABLE model_turn_admission_schema (
    marker VARCHAR(64) PRIMARY KEY DEFAULT 'model_turn_admission_schema',
    version BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT model_turn_admission_schema_marker CHECK (marker = 'model_turn_admission_schema'),
    CONSTRAINT model_turn_admission_schema_version CHECK (version = 1)
);
INSERT INTO model_turn_admission_schema (marker, version)
VALUES ('model_turn_admission_schema', 1);

CREATE TABLE model_turn_pools (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    credential_id VARCHAR(36) NOT NULL REFERENCES credentials(id) ON DELETE RESTRICT,
    provider_id VARCHAR(191) NOT NULL,
    model_id VARCHAR(191) NOT NULL,
    phase VARCHAR(16) NOT NULL DEFAULT 'off',
    identity_state VARCHAR(32) NOT NULL DEFAULT 'eligible',
    capability_state VARCHAR(32) NOT NULL DEFAULT 'unknown',
    learned_concurrency BIGINT NOT NULL DEFAULT 0,
    in_flight BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT model_turn_pools_scope_unique UNIQUE (credential_id, provider_id, model_id),
    CONSTRAINT model_turn_pools_provider_nonempty CHECK (btrim(provider_id) <> ''),
    CONSTRAINT model_turn_pools_model_nonempty CHECK (btrim(model_id) <> ''),
    CONSTRAINT model_turn_pools_phase_valid CHECK (phase IN ('off', 'shadow', 'draining', 'enforce')),
    CONSTRAINT model_turn_pools_identity_valid CHECK (identity_state IN ('eligible', 'revoked', 'ambiguous', 'colliding')),
    CONSTRAINT model_turn_pools_capability_valid CHECK (capability_state IN ('unknown', 'supported', 'unsupported', 'degraded')),
    CONSTRAINT model_turn_pools_accounting_nonnegative CHECK (learned_concurrency >= 0 AND in_flight >= 0)
);
CREATE INDEX model_turn_pools_credential_scope_idx ON model_turn_pools (credential_id, provider_id, model_id);

CREATE TABLE model_turn_bucket_bindings (
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    bucket_kind VARCHAR(16) NOT NULL,
    capacity_units BIGINT NOT NULL,
    available_units BIGINT NOT NULL,
    authoritative_epoch BIGINT NOT NULL DEFAULT 0,
    reset_at TIMESTAMPTZ,
    quarantined_units BIGINT NOT NULL DEFAULT 0,
    observation_sequence BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (pool_id, bucket_kind),
    CONSTRAINT model_turn_bucket_kind_valid CHECK (bucket_kind IN ('request', 'input', 'output', 'combined')),
    CONSTRAINT model_turn_bucket_accounting_nonnegative CHECK (
        capacity_units >= 0 AND available_units >= 0 AND available_units <= capacity_units
        AND authoritative_epoch >= 0 AND quarantined_units >= 0 AND observation_sequence >= 0
    )
);
CREATE INDEX model_turn_bucket_bindings_reset_idx ON model_turn_bucket_bindings (pool_id, reset_at);

CREATE TABLE model_turn_reservations (
    id UUID PRIMARY KEY,
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE RESTRICT,
    request_id VARCHAR(128) NOT NULL,
    state VARCHAR(16) NOT NULL DEFAULT 'reserved',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    terminal_at TIMESTAMPTZ,
    CONSTRAINT model_turn_reservations_pool_request_unique UNIQUE (pool_id, request_id),
    -- These composite identities fence the duplicated pool/request columns in
    -- bucket debits and leases to this reservation's own pool and request.
    CONSTRAINT model_turn_reservations_id_pool_unique UNIQUE (id, pool_id),
    CONSTRAINT model_turn_reservations_id_pool_request_unique UNIQUE (id, pool_id, request_id),
    CONSTRAINT model_turn_reservations_request_nonempty CHECK (btrim(request_id) <> ''),
    CONSTRAINT model_turn_reservations_state_valid CHECK (state IN ('reserved', 'dispatched', 'reconciled', 'expired', 'cancelled')),
    CONSTRAINT model_turn_reservations_terminal_lifecycle CHECK (
        (state IN ('reserved', 'dispatched') AND terminal_at IS NULL)
        OR (state IN ('reconciled', 'expired', 'cancelled') AND terminal_at IS NOT NULL)
    )
);
CREATE TABLE model_turn_reservation_buckets (
    reservation_id UUID NOT NULL,
    pool_id BIGINT NOT NULL,
    bucket_kind VARCHAR(16) NOT NULL,
    reserved_units BIGINT NOT NULL,
    PRIMARY KEY (reservation_id, bucket_kind),
    CONSTRAINT model_turn_reservation_buckets_reservation_pool_fk
        FOREIGN KEY (reservation_id, pool_id) REFERENCES model_turn_reservations(id, pool_id) ON DELETE CASCADE,
    CONSTRAINT model_turn_reservation_buckets_binding_fk
        FOREIGN KEY (pool_id, bucket_kind) REFERENCES model_turn_bucket_bindings(pool_id, bucket_kind) ON DELETE RESTRICT,
    CONSTRAINT model_turn_reservation_buckets_units_nonnegative CHECK (reserved_units >= 0)
);

CREATE TABLE model_turn_leases (
    lease_id UUID PRIMARY KEY,
    generation BIGINT NOT NULL,
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE RESTRICT,
    reservation_id UUID NOT NULL UNIQUE,
    request_id VARCHAR(128) NOT NULL,
    owner_pod_uid VARCHAR(255),
    lifecycle VARCHAR(16) NOT NULL DEFAULT 'reserved',
    reserved_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    dispatching_at TIMESTAMPTZ,
    active_at TIMESTAMPTZ,
    heartbeat_at TIMESTAMPTZ,
    terminal_at TIMESTAMPTZ,
    CONSTRAINT model_turn_leases_reservation_identity_fk
        FOREIGN KEY (reservation_id, pool_id, request_id)
        REFERENCES model_turn_reservations(id, pool_id, request_id) ON DELETE RESTRICT,
    CONSTRAINT model_turn_leases_generation_positive CHECK (generation > 0),
    CONSTRAINT model_turn_leases_request_nonempty CHECK (btrim(request_id) <> ''),
    CONSTRAINT model_turn_leases_lifecycle_valid CHECK (lifecycle IN ('reserved', 'dispatching', 'active', 'reconciled', 'expired')),
    CONSTRAINT model_turn_leases_terminal_lifecycle CHECK (
        (lifecycle IN ('reserved', 'dispatching', 'active') AND terminal_at IS NULL)
        OR (lifecycle IN ('reconciled', 'expired') AND terminal_at IS NOT NULL)
    )
);
CREATE UNIQUE INDEX model_turn_leases_pool_request_idx ON model_turn_leases (pool_id, request_id);
CREATE INDEX model_turn_leases_lifecycle_heartbeat_idx ON model_turn_leases (lifecycle, heartbeat_at);

-- One terminal record per lease makes terminal reconciliation idempotent at the
-- durable boundary. Detail is intentionally a bounded, non-secret diagnostic.
CREATE TABLE model_turn_lease_terminals (
    lease_id UUID PRIMARY KEY REFERENCES model_turn_leases(lease_id) ON DELETE RESTRICT,
    generation BIGINT NOT NULL,
    request_id VARCHAR(128) NOT NULL,
    outcome VARCHAR(32) NOT NULL,
    detail VARCHAR(1024),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT model_turn_lease_terminals_generation_positive CHECK (generation > 0),
    CONSTRAINT model_turn_lease_terminals_outcome_valid CHECK (outcome IN ('completed', 'cancelled', 'expired', 'failed')),
    CONSTRAINT model_turn_lease_terminals_detail_bounded CHECK (detail IS NULL OR char_length(detail) <= 1024)
);

CREATE TABLE model_turn_controller_windows (
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    window_sequence BIGINT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ NOT NULL,
    admitted_turns BIGINT NOT NULL DEFAULT 0,
    completed_turns BIGINT NOT NULL DEFAULT 0,
    summary VARCHAR(2048),
    PRIMARY KEY (pool_id, window_sequence),
    CONSTRAINT model_turn_controller_windows_sequence_nonnegative CHECK (window_sequence >= 0),
    CONSTRAINT model_turn_controller_windows_counts_nonnegative CHECK (admitted_turns >= 0 AND completed_turns >= 0),
    CONSTRAINT model_turn_controller_windows_time_order CHECK (ended_at >= started_at),
    CONSTRAINT model_turn_controller_windows_summary_bounded CHECK (summary IS NULL OR char_length(summary) <= 2048)
);

CREATE TABLE model_turn_pool_capabilities (
    pool_id BIGINT PRIMARY KEY REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    capability_state VARCHAR(32) NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    detail VARCHAR(1024),
    CONSTRAINT model_turn_pool_capabilities_state_valid CHECK (capability_state IN ('unknown', 'supported', 'unsupported', 'degraded')),
    CONSTRAINT model_turn_pool_capabilities_detail_bounded CHECK (detail IS NULL OR char_length(detail) <= 1024)
);

CREATE TABLE model_turn_observations (
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    sequence BIGINT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind VARCHAR(32) NOT NULL,
    request_units BIGINT NOT NULL DEFAULT 0,
    input_units BIGINT NOT NULL DEFAULT 0,
    output_units BIGINT NOT NULL DEFAULT 0,
    detail VARCHAR(1024),
    PRIMARY KEY (pool_id, sequence),
    CONSTRAINT model_turn_observations_sequence_nonnegative CHECK (sequence >= 0),
    CONSTRAINT model_turn_observations_kind_valid CHECK (kind IN ('usage', 'rate_limit', 'reset', 'capability', 'quarantine')),
    CONSTRAINT model_turn_observations_units_nonnegative CHECK (request_units >= 0 AND input_units >= 0 AND output_units >= 0),
    CONSTRAINT model_turn_observations_detail_bounded CHECK (detail IS NULL OR char_length(detail) <= 1024)
);
CREATE INDEX model_turn_observations_pool_observed_idx ON model_turn_observations (pool_id, observed_at DESC);

-- Retain a bounded rolling telemetry window per pool rather than permitting an
-- unbounded attempt-event ledger. The monotonic sequence remains durable even
-- after older observations are trimmed.
CREATE OR REPLACE FUNCTION trim_model_turn_observations() RETURNS trigger AS $$
BEGIN
    -- Locking the parent serializes every per-pool trim. Without it, concurrent
    -- AFTER INSERT triggers can each see only their own uncommitted row and
    -- collectively retain more than the 256-row bound.
    PERFORM 1 FROM model_turn_pools WHERE id = NEW.pool_id FOR UPDATE;
    DELETE FROM model_turn_observations
     WHERE pool_id = NEW.pool_id
       AND sequence IN (
           SELECT sequence FROM model_turn_observations
            WHERE pool_id = NEW.pool_id
            ORDER BY sequence DESC
            OFFSET 256
       );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER model_turn_observations_bounded
AFTER INSERT ON model_turn_observations
FOR EACH ROW EXECUTE FUNCTION trim_model_turn_observations();
