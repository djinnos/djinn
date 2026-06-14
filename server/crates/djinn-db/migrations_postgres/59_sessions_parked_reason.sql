-- Add nullable reason marker for sessions deliberately parked after completion.
ALTER TABLE sessions
    ADD COLUMN parked_reason text NULL;

CREATE INDEX idx_sessions_parked_reason
    ON sessions(parked_reason)
    WHERE parked_reason IS NOT NULL;
