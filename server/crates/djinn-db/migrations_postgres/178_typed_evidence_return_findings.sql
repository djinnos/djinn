-- Retain conclusions and their independently hydrated anchors as immutable,
-- normalized validation facts rather than treating them as a transport detail.
CREATE TABLE typed_evidence_return_findings (
    id VARCHAR(36) PRIMARY KEY,
    validation_result_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_validation_results(id) ON DELETE RESTRICT,
    planned_check_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_planned_checks(id) ON DELETE RESTRICT,
    conclusion TEXT NOT NULL,
    usable BOOLEAN NOT NULL,
    UNIQUE (validation_result_id, planned_check_id)
);
CREATE TABLE typed_evidence_return_finding_anchors (
    id VARCHAR(36) PRIMARY KEY,
    finding_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_return_findings(id) ON DELETE RESTRICT,
    method VARCHAR(32) NOT NULL,
    locator TEXT NOT NULL,
    health VARCHAR(16) NOT NULL,
    immutable_identity JSONB NOT NULL,
    detail TEXT NULL,
    CONSTRAINT typed_evidence_return_finding_anchor_method CHECK (method IN ('code','graph','command','artifact','memory','external','repository')),
    CONSTRAINT typed_evidence_return_finding_anchor_health CHECK (health IN ('healthy','unusable','unavailable')),
    CONSTRAINT typed_evidence_return_finding_anchor_identity CHECK (jsonb_typeof(immutable_identity) = 'object')
);
