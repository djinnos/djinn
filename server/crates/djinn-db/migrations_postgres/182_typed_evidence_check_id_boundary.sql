-- TribunalEvidenceReturnV1 permits bounded check identities up to 2 KiB.
-- Keep plan-backed identities and their foreign-key children equally wide.
ALTER TABLE evidence_command_invocations
    DROP CONSTRAINT evidence_command_invocations_check_fk;
ALTER TABLE typed_evidence_planned_checks
    DROP CONSTRAINT typed_evidence_planned_checks_plan_check_fk;

ALTER TABLE evidence_plan_checks
    ALTER COLUMN check_id TYPE VARCHAR(2048);
ALTER TABLE evidence_command_invocations
    ALTER COLUMN check_id TYPE VARCHAR(2048);
ALTER TABLE typed_evidence_planned_checks
    ALTER COLUMN check_id TYPE VARCHAR(2048),
    ALTER COLUMN evidence_plan_check_id TYPE VARCHAR(2048);

ALTER TABLE evidence_command_invocations
    ADD CONSTRAINT evidence_command_invocations_check_fk
    FOREIGN KEY (plan_id, check_id)
    REFERENCES evidence_plan_checks (plan_id, check_id);
ALTER TABLE typed_evidence_planned_checks
    ADD CONSTRAINT typed_evidence_planned_checks_plan_check_fk
    FOREIGN KEY (evidence_plan_id, evidence_plan_check_id)
    REFERENCES evidence_plan_checks (plan_id, check_id) ON DELETE RESTRICT;
