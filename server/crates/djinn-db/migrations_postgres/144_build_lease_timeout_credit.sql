-- A queue deadline may give its owner one bounded retry credit. Keeping the
-- consumption bit with the terminal row makes lost-response replay safe.
ALTER TABLE build_leases
    ADD COLUMN IF NOT EXISTS timeout_credit_consumed BOOLEAN NOT NULL DEFAULT FALSE;
