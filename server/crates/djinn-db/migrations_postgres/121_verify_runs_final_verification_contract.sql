-- Migration 121: durable final-verification verify-run contract.
-- This is deliberately additive: legacy audit columns remain readable.

ALTER TABLE verify_runs
    ADD COLUMN IF NOT EXISTS source_phase VARCHAR(64) NULL,
    ADD COLUMN IF NOT EXISTS verification_attempt_id VARCHAR(255) NULL,
    ADD COLUMN IF NOT EXISTS ordered_commands JSONB NULL,
    ADD COLUMN IF NOT EXISTS covered_checks JSONB NULL,
    ADD COLUMN IF NOT EXISTS verification_input_fingerprint VARCHAR(255) NULL,
    ADD COLUMN IF NOT EXISTS manifest_version VARCHAR(128) NULL,
    ADD COLUMN IF NOT EXISTS environment_identity_json JSONB NULL,
    ADD COLUMN IF NOT EXISTS environment_identity_digest VARCHAR(255) NULL,
    ADD COLUMN IF NOT EXISTS environment_identity_version VARCHAR(64) NULL;

CREATE INDEX IF NOT EXISTS idx_verify_runs_final_verification_lookup
    ON verify_runs (
        verification_input_fingerprint,
        manifest_version,
        environment_identity_version,
        environment_identity_digest,
        created_at DESC
    )
    WHERE source_phase = 'final_verification'
      AND result = 'pass'
      AND environment_identity_digest IS NOT NULL;
