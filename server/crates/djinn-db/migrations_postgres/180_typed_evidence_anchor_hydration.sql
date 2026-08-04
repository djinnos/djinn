-- Exact, server-authored anchor identities. No caller locator is searched inside
-- finalized JSON; a source must be explicitly registered for one attempt/check.
CREATE TABLE typed_evidence_anchor_sources (
    id VARCHAR(36) PRIMARY KEY,
    attempt_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_attempts(id) ON DELETE RESTRICT,
    planned_check_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_planned_checks(id) ON DELETE RESTRICT,
    family VARCHAR(32) NOT NULL,
    locator TEXT NOT NULL,
    immutable_identity JSONB NOT NULL,
    health VARCHAR(16) NOT NULL,
    detail TEXT NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    UNIQUE (attempt_id, planned_check_id, family, locator),
    CONSTRAINT typed_evidence_anchor_source_family CHECK (family IN ('code','graph','command','artifact','memory','external','repository')),
    CONSTRAINT typed_evidence_anchor_source_locator CHECK (length(btrim(locator)) > 0),
    CONSTRAINT typed_evidence_anchor_source_identity CHECK (jsonb_typeof(immutable_identity) = 'object'),
    CONSTRAINT typed_evidence_anchor_source_health CHECK (health IN ('healthy','unusable','unavailable'))
);
CREATE OR REPLACE FUNCTION reject_typed_evidence_anchor_source_mutation() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'typed evidence anchor sources are immutable';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER typed_evidence_anchor_sources_append_only
    BEFORE UPDATE OR DELETE ON typed_evidence_anchor_sources
    FOR EACH ROW EXECUTE FUNCTION reject_typed_evidence_anchor_source_mutation();

ALTER TABLE typed_evidence_anchor_health
    ADD COLUMN immutable_identity JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN method_compatible BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE typed_evidence_return_finding_anchors
    ADD COLUMN method_compatible BOOLEAN NOT NULL DEFAULT FALSE;
