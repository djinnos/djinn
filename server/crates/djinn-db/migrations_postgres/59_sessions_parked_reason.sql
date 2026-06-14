-- Persist deliberate session park reason metadata (for example budget parks)
-- without adding new values to the session status enum/string lifecycle.
ALTER TABLE sessions
    ADD COLUMN parked_reason text NULL;

CREATE INDEX idx_sessions_parked_reason
    ON sessions(parked_reason)
    WHERE parked_reason IS NOT NULL;
