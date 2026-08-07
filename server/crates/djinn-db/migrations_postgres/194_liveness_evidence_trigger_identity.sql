-- Migration 194: durable internal identity for insert-once session-exit
-- liveness observations. This is deliberately nullable: existing and
-- non-exit classification snapshots remain append-only and readable.

ALTER TABLE liveness_evidence
    ADD COLUMN IF NOT EXISTS trigger_identity TEXT NULL;

-- A partial unique index preserves multiple NULL (legacy/non-exit) rows while
-- making one durable `session_exit:{session_id}` delivery immutable.
CREATE UNIQUE INDEX IF NOT EXISTS idx_liveness_evidence_trigger_identity_unique
    ON liveness_evidence(trigger_identity)
    WHERE trigger_identity IS NOT NULL;
