-- Per-proposal/per-epic reconciliation revision stamps for amend-while-building.
-- Stores which graduated epic has been reconciled against which proposal
-- revision without denormalizing this state onto epics.
CREATE TABLE proposal_reconciliations (
    proposal_id  VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    epic_id      VARCHAR(36) NOT NULL REFERENCES epics(id) ON DELETE CASCADE,
    revision_seq INT NOT NULL,
    created_at   VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at   VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (proposal_id, epic_id, revision_seq),
    FOREIGN KEY (proposal_id, epic_id) REFERENCES proposal_epics(proposal_id, epic_id) ON DELETE CASCADE
);

CREATE INDEX proposal_reconciliations_proposal_epic
    ON proposal_reconciliations(proposal_id, epic_id, revision_seq DESC);
