-- Post-cutoff feedback must survive a live refinement generation. Rows are
-- replayable by feedback boundary and exactly one pending row owns a cohort.
CREATE TABLE pending_feedback_refinement_handoffs (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    proposal_id VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    boundary_feedback_id VARCHAR(36) NOT NULL REFERENCES proposal_feedback(id) ON DELETE CASCADE,
    state VARCHAR(16) NOT NULL DEFAULT 'pending',
    cohort_owner BOOLEAN NOT NULL DEFAULT FALSE,
    successor_run_id VARCHAR(36) NULL REFERENCES refinement_runs(id) ON DELETE SET NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT pending_feedback_refinement_handoffs_state CHECK (state IN ('pending', 'admitted')),
    CONSTRAINT pending_feedback_refinement_handoffs_boundary UNIQUE (proposal_id, boundary_feedback_id)
);
CREATE UNIQUE INDEX pending_feedback_refinement_handoffs_one_owner
    ON pending_feedback_refinement_handoffs(proposal_id)
    WHERE state = 'pending' AND cohort_owner;
