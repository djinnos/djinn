-- Persist deterministic lint output for immutable proposal revision snapshots.
--
-- A lint result belongs to one concrete proposal_revisions row. The public
-- lookup key remains (proposal_id, revision_seq, linter_version), while
-- revision_id makes the relationship unambiguous even though proposal history
-- also contains non-snapshot lifecycle events at an existing sequence.

-- `id` is already the primary key, but this composite unique index is required
-- for the composite FK below. It also ensures proposal/revision coordinates on
-- a lint row cannot point at a different revision id.
CREATE UNIQUE INDEX IF NOT EXISTS proposal_revisions_id_proposal_id_seq_unique
    ON proposal_revisions (id, proposal_id, seq);

CREATE TABLE IF NOT EXISTS proposal_revision_lint_results (
    proposal_id    VARCHAR(36)  NOT NULL,
    revision_seq   INT          NOT NULL,
    linter_version VARCHAR(255) NOT NULL,
    revision_id    VARCHAR(36)  NOT NULL,
    body_sha256    VARCHAR(64)  NOT NULL,
    result_json    JSONB        NOT NULL,
    PRIMARY KEY (proposal_id, revision_seq, linter_version),
    CONSTRAINT proposal_revision_lint_results_revision_fk
        FOREIGN KEY (revision_id, proposal_id, revision_seq)
        REFERENCES proposal_revisions (id, proposal_id, seq)
        ON DELETE CASCADE
);

-- Doctor findings created before this migration have no deduplication key.
-- Only supplied keys participate in uniqueness, so legacy and ad-hoc findings
-- can continue to use NULL.
ALTER TABLE doctor_findings
    ADD COLUMN IF NOT EXISTS deduplication_key VARCHAR(255) NULL;

CREATE UNIQUE INDEX IF NOT EXISTS doctor_findings_deduplication_key_unique
    ON doctor_findings (deduplication_key)
    WHERE deduplication_key IS NOT NULL;
