-- Immutable provenance retained for each normalized TribunalEvidenceReturnV1 check.
CREATE TABLE typed_evidence_invocation_provenance (
    validation_result_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_validation_results(id) ON DELETE RESTRICT,
    check_result_id VARCHAR(36) NOT NULL REFERENCES typed_evidence_check_results(id) ON DELETE RESTRICT,
    invocation_id VARCHAR(36) NOT NULL REFERENCES evidence_command_invocations(id) ON DELETE RESTRICT,
    usable BOOLEAN NOT NULL,
    PRIMARY KEY (validation_result_id, check_result_id),
    UNIQUE (invocation_id, check_result_id)
);
