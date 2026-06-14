-- Proposal revision history events.
--
-- Preserve existing spec-revision semantics while allowing lifecycle-only
-- history entries (currently manual status changes to done) to live in the same
-- chronological proposal history stream. Status events reuse the current spec
-- revision sequence and therefore must not participate in the historical
-- (proposal_id, seq) uniqueness constraint for spec snapshots.

ALTER TABLE proposal_revisions
    ADD COLUMN event_kind VARCHAR(32) NOT NULL DEFAULT 'spec_revision',
    ADD COLUMN status_from VARCHAR(64) NULL,
    ADD COLUMN status_to VARCHAR(64) NULL;

ALTER TABLE proposal_revisions
    DROP CONSTRAINT IF EXISTS proposal_revisions_proposal_id_seq_key;

CREATE UNIQUE INDEX IF NOT EXISTS proposal_revisions_spec_revision_seq_unique
    ON proposal_revisions(proposal_id, seq)
    WHERE event_kind = 'spec_revision';

CREATE INDEX IF NOT EXISTS proposal_revisions_proposal_chronological
    ON proposal_revisions(proposal_id, created_at, id);
