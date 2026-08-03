-- A cold capability state has one durable discovery owner. This is separate
-- from leases because discovery occurs before admission reserves a lease.
CREATE TABLE model_turn_capability_discoveries (
    pool_id BIGINT PRIMARY KEY REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    owner_request_id VARCHAR(128) NOT NULL,
    owner_pod_uid VARCHAR(255),
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT model_turn_capability_discoveries_request_nonempty
        CHECK (btrim(owner_request_id) <> '')
);
