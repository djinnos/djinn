-- Migration 156: retain confidence from accepted readiness findings.

ALTER TABLE readiness_guardrail_findings
    ADD COLUMN confidence DOUBLE PRECISION NOT NULL DEFAULT 0,
    ADD CONSTRAINT readiness_findings_confidence_check
        CHECK (confidence >= 0 AND confidence <= 1);
