-- Add a separate counter for consecutive verification failures.
-- Resets to 0 on VerificationPass; incremented on VerificationFail.
-- After 3 consecutive failures the task escalates to PM intervention.
ALTER TABLE tasks ADD COLUMN verification_failure_count INTEGER NOT NULL DEFAULT 0;
