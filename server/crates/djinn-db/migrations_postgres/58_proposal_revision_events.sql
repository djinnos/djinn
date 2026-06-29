-- Extend proposal_revisions from spec snapshots into a minimal proposal
-- history/audit stream. Existing rows are spec revisions; status-only events
-- can be appended at the current spec seq without advancing
-- proposals.latest_revision_seq.

ALTER TABLE proposal_revisions
    -- Longest current lifecycle event kind is
    -- `refinement_awaiting_evidence_started` (38 chars), so fresh databases
    -- need the base column wider than the original status-event names.
    ADD COLUMN event_kind VARCHAR(64) NOT NULL DEFAULT 'spec_revision',
    ADD COLUMN status_from VARCHAR(64) NULL,
    ADD COLUMN status_to VARCHAR(64) NULL,
    ADD COLUMN event_metadata JSONB NULL;

-- The old table-level unique constraint prevented status/audit events from
-- sharing the current spec seq. Keep spec revision sequencing unique without
-- making status history participate in revision numbering.
ALTER TABLE proposal_revisions DROP CONSTRAINT IF EXISTS proposal_revisions_proposal_id_seq_key;

CREATE UNIQUE INDEX IF NOT EXISTS proposal_revisions_spec_seq_unique
    ON proposal_revisions(proposal_id, seq)
    WHERE event_kind = 'spec_revision';

CREATE INDEX IF NOT EXISTS proposal_revisions_proposal_created_at
    ON proposal_revisions(proposal_id, created_at, id);
