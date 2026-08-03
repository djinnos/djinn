-- A terminal outcome is immutable, but a provider can report authoritative
-- usage after an earlier missing-usage terminalization quarantined the debit.
-- This state makes that one later accounting transition durable and fenced.
ALTER TABLE model_turn_lease_terminals
    ADD COLUMN accounting_state VARCHAR(16) NOT NULL DEFAULT 'pending',
    ADD CONSTRAINT model_turn_lease_terminals_accounting_state_valid
        CHECK (accounting_state IN ('pending', 'refunded', 'quarantined', 'authoritative'));
