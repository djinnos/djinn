-- Bounded Phase-C evidence. This is intentionally route-qualified through the
-- existing model_turn_pools ledger and stores no request, lease, credential, or user identity.
CREATE TABLE model_turn_capability_heartbeats (
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    slot_pod_uid VARCHAR(255) NOT NULL,
    deployment_revision VARCHAR(255) NOT NULL,
    provider_id VARCHAR(191) NOT NULL,
    model_id VARCHAR(191) NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (pool_id, slot_pod_uid, deployment_revision),
    CONSTRAINT model_turn_capability_heartbeats_slot_nonempty CHECK (btrim(slot_pod_uid) <> ''),
    CONSTRAINT model_turn_capability_heartbeats_revision_nonempty CHECK (btrim(deployment_revision) <> '')
);
CREATE INDEX model_turn_capability_heartbeats_pool_recent_idx
    ON model_turn_capability_heartbeats (pool_id, heartbeat_at DESC);

CREATE TABLE model_turn_phase_c_evidence (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pool_id BIGINT NOT NULL REFERENCES model_turn_pools(id) ON DELETE CASCADE,
    slot_pod_uid VARCHAR(255) NOT NULL,
    deployment_revision VARCHAR(255) NOT NULL,
    provider_id VARCHAR(191) NOT NULL,
    model_id VARCHAR(191) NOT NULL,
    attempt_fingerprint VARCHAR(71) NOT NULL,
    stage VARCHAR(32) NOT NULL,
    outcome VARCHAR(32) NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT model_turn_phase_c_evidence_slot_nonempty CHECK (btrim(slot_pod_uid) <> ''),
    CONSTRAINT model_turn_phase_c_evidence_revision_nonempty CHECK (btrim(deployment_revision) <> ''),
    CONSTRAINT model_turn_phase_c_evidence_fingerprint CHECK (attempt_fingerprint ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT model_turn_phase_c_evidence_stage CHECK (stage IN ('decision', 'dispatch', 'heartbeat', 'provider_outcome', 'reconcile')),
    CONSTRAINT model_turn_phase_c_evidence_outcome CHECK (outcome IN ('recorded', 'succeeded', 'failed', 'missing'))
);
CREATE INDEX model_turn_phase_c_evidence_pool_recent_idx
    ON model_turn_phase_c_evidence (pool_id, recorded_at DESC);

CREATE OR REPLACE FUNCTION trim_model_turn_phase_c_evidence() RETURNS trigger AS $$
BEGIN
    PERFORM 1 FROM model_turn_pools WHERE id = NEW.pool_id FOR UPDATE;
    DELETE FROM model_turn_phase_c_evidence WHERE id IN (
        SELECT id FROM model_turn_phase_c_evidence WHERE pool_id = NEW.pool_id
        ORDER BY recorded_at DESC, id DESC OFFSET 256
    );
    DELETE FROM model_turn_capability_heartbeats WHERE ctid IN (
        SELECT ctid FROM model_turn_capability_heartbeats WHERE pool_id = NEW.pool_id
        ORDER BY heartbeat_at DESC OFFSET 256
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER model_turn_phase_c_evidence_bounded
AFTER INSERT ON model_turn_phase_c_evidence
FOR EACH ROW EXECUTE FUNCTION trim_model_turn_phase_c_evidence();
CREATE TRIGGER model_turn_capability_heartbeats_bounded
AFTER INSERT OR UPDATE ON model_turn_capability_heartbeats
FOR EACH ROW EXECUTE FUNCTION trim_model_turn_phase_c_evidence();
