-- Immutable V1 lint provenance for accepted proposal revision snapshots.
CREATE TABLE proposal_revision_lint_results (
    proposal_id VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    revision_id VARCHAR(36) NOT NULL REFERENCES proposal_revisions(id) ON DELETE CASCADE,
    linter_version VARCHAR(32) NOT NULL,
    body_sha256 VARCHAR(64) NOT NULL,
    result JSONB NOT NULL,
    checked_at VARCHAR(64) NOT NULL,
    PRIMARY KEY (proposal_id, revision_id, linter_version)
);
CREATE INDEX proposal_revision_lint_results_revision_id ON proposal_revision_lint_results (revision_id);
