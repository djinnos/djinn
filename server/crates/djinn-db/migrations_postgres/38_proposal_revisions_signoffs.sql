-- Phase 1, Step 2 — proposal revisions + sign-offs.
--
-- Every material edit (title/body/acceptance_criteria) appends an immutable
-- revision snapshot; `proposals.latest_revision_seq` tracks the head. Sign-offs
-- (scoped = product, technical = engineering) anchor to the revision they were
-- given against, so "stale" = signoff.revision_seq < latest_revision_seq and
-- "changes since your approval" is the diff between those two revisions.

ALTER TABLE proposals ADD COLUMN latest_revision_seq INT NOT NULL DEFAULT 0;

CREATE TABLE proposal_revisions (
    id                  VARCHAR(36) NOT NULL PRIMARY KEY,
    proposal_id         VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    seq                 INT         NOT NULL,
    title               VARCHAR(512) NOT NULL,
    body                TEXT        NOT NULL,
    acceptance_criteria JSONB       NOT NULL DEFAULT '[]',
    edited_by_user_id   VARCHAR(36) NULL,
    created_at          VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    UNIQUE (proposal_id, seq)
);

CREATE INDEX proposal_revisions_proposal_id ON proposal_revisions(proposal_id);

CREATE TABLE proposal_signoffs (
    proposal_id  VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    kind         VARCHAR(16) NOT NULL,            -- scoped | technical
    user_id      VARCHAR(36) NOT NULL,
    revision_seq INT         NOT NULL,            -- revision this sign-off was given against
    created_at   VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (proposal_id, kind, user_id)
);

CREATE INDEX proposal_signoffs_proposal_id ON proposal_signoffs(proposal_id);

-- Backfill an initial revision (seq 1) for existing proposals so every proposal
-- has a head revision to diff against.
INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, acceptance_criteria, edited_by_user_id, created_at)
    SELECT gen_random_uuid()::text, id, 1, title, body, acceptance_criteria, author_user_id, updated_at
    FROM proposals;

UPDATE proposals SET latest_revision_seq = 1;
