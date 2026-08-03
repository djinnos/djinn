-- Dormant persistence contract for feedback-derived refinement. Existing feedback
-- remains blocking by default and legacy writers need not know about any of these
-- columns or tables.
ALTER TABLE proposal_feedback
    ADD COLUMN severity VARCHAR(16) NOT NULL DEFAULT 'blocking',
    ADD COLUMN withdrawn_at VARCHAR(64) NULL,
    ADD COLUMN withdrawn_by_user_id VARCHAR(36) NULL,
    ADD CONSTRAINT proposal_feedback_severity_check
        CHECK (severity IN ('blocking', 'advisory')),
    ADD CONSTRAINT proposal_feedback_withdrawal_check
        CHECK (
            (withdrawn_at IS NULL AND withdrawn_by_user_id IS NULL)
            OR (withdrawn_at IS NOT NULL AND withdrawn_by_user_id IS NOT NULL)
        );

CREATE INDEX proposal_feedback_actionable_blocking_idx
    ON proposal_feedback (proposal_id, parent_id, created_at)
    WHERE severity = 'blocking' AND resolved_at IS NULL AND withdrawn_at IS NULL;

-- A root-scoped, materialized generation. The captured boundary is immutable:
-- later feedback is deliberately absent from this record and belongs to a later
-- generation. State changes are limited to the disposition lifecycle.
CREATE TABLE proposal_feedback_refinement_injections (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    proposal_id VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    root_feedback_id VARCHAR(36) NOT NULL REFERENCES proposal_feedback(id) ON DELETE RESTRICT,
    generation INT NOT NULL,
    state VARCHAR(32) NOT NULL DEFAULT 'queued',
    cutoff_at VARCHAR(64) NOT NULL,
    cutoff_feedback_id VARCHAR(36) NOT NULL REFERENCES proposal_feedback(id) ON DELETE RESTRICT,
    round INT NOT NULL,
    debate_entry_id VARCHAR(36) NULL REFERENCES proposal_debate_trail(id) ON DELETE RESTRICT,
    accepted_disposition VARCHAR(32) NULL,
    accepted_revision_seq INT NULL,
    accepted_at VARCHAR(64) NULL,
    accepted_by_user_id VARCHAR(36) NULL,
    created_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT proposal_feedback_refinement_injections_generation_unique
        UNIQUE (root_feedback_id, generation),
    CONSTRAINT proposal_feedback_refinement_injections_generation_check
        CHECK (generation > 0),
    CONSTRAINT proposal_feedback_refinement_injections_round_check
        CHECK (round > 0),
    CONSTRAINT proposal_feedback_refinement_injections_state_check
        CHECK (state IN ('queued', 'injected', 'accepted', 'wont_fix', 'withdrawn_by_author')),
    CONSTRAINT proposal_feedback_refinement_injections_disposition_check
        CHECK (
            (state IN ('queued', 'injected', 'withdrawn_by_author')
                AND accepted_disposition IS NULL
                AND accepted_revision_seq IS NULL
                AND accepted_at IS NULL
                AND accepted_by_user_id IS NULL)
            OR (state = 'accepted'
                AND accepted_disposition = 'fixed_revision'
                AND accepted_revision_seq IS NOT NULL
                AND accepted_at IS NOT NULL)
            OR (state = 'wont_fix'
                AND accepted_disposition = 'wont_fix'
                AND accepted_revision_seq IS NULL
                AND accepted_at IS NOT NULL)
        )
);

-- A debate entry can only materialize one feedback generation. NULL remains
-- available while an injector has claimed a queued generation but has not yet
-- persisted the corresponding debate entry.
CREATE UNIQUE INDEX proposal_feedback_refinement_injections_debate_entry_unique
    ON proposal_feedback_refinement_injections (debate_entry_id)
    WHERE debate_entry_id IS NOT NULL;
CREATE INDEX proposal_feedback_refinement_injections_proposal_state_idx
    ON proposal_feedback_refinement_injections (proposal_id, state, round, created_at);

-- Verbatim source snapshots are append-only membership records. They retain the
-- exact author/body/severity observed at the cutoff, rather than depending on a
-- later mutable feedback row, and source_ordinal supplies deterministic debate
-- prompt ordering.
CREATE TABLE proposal_feedback_refinement_sources (
    injection_id VARCHAR(36) NOT NULL REFERENCES proposal_feedback_refinement_injections(id) ON DELETE CASCADE,
    source_feedback_id VARCHAR(36) NOT NULL REFERENCES proposal_feedback(id) ON DELETE RESTRICT,
    source_ordinal INT NOT NULL,
    source_parent_id VARCHAR(36) NULL,
    source_author_kind VARCHAR(16) NOT NULL,
    source_author_user_id VARCHAR(36) NULL,
    source_author_model VARCHAR(128) NULL,
    source_body TEXT NOT NULL,
    source_severity VARCHAR(16) NOT NULL,
    source_created_at VARCHAR(64) NOT NULL,
    captured_at VARCHAR(64) NOT NULL,
    PRIMARY KEY (injection_id, source_feedback_id),
    CONSTRAINT proposal_feedback_refinement_sources_ordinal_unique
        UNIQUE (injection_id, source_ordinal),
    CONSTRAINT proposal_feedback_refinement_sources_ordinal_check
        CHECK (source_ordinal > 0),
    CONSTRAINT proposal_feedback_refinement_sources_severity_check
        CHECK (source_severity IN ('blocking', 'advisory'))
);
CREATE INDEX proposal_feedback_refinement_sources_feedback_idx
    ON proposal_feedback_refinement_sources (source_feedback_id);

-- `human_feedback` is dormant until the capture repository starts producing it;
-- expanding the existing check keeps those future linked entries valid without
-- changing legacy debate rows.
ALTER TABLE proposal_debate_trail
    DROP CONSTRAINT proposal_debate_trail_kind_check,
    ADD CONSTRAINT proposal_debate_trail_kind_check
        CHECK (kind IN ('objection', 'rebuttal', 'verdict', 'needs_evidence', 'evidence_findings', 'human_feedback'));
