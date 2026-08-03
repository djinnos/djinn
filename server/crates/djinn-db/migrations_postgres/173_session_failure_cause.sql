-- Durable, machine-readable cause for failed or interrupted sessions. The value
-- deliberately excludes diagnostic text and legacy read-time interpretations.
ALTER TABLE sessions
    ADD COLUMN failure_cause TEXT NULL,
    ADD CONSTRAINT sessions_failure_cause_valid
        CHECK (failure_cause IS NULL OR failure_cause IN (
            'cancelled',
            'provider',
            'harness',
            'infrastructure',
            'protocol',
            'finalization',
            'unknown'
        ));
