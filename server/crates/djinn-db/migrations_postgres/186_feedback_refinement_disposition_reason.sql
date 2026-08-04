-- Preserve the Judge's non-empty wont-fix rationale as part of disposition identity.
ALTER TABLE proposal_feedback_refinement_injections
    ADD COLUMN accepted_reason TEXT NULL;

ALTER TABLE proposal_feedback_refinement_injections
    DROP CONSTRAINT proposal_feedback_refinement_injections_disposition_check,
    ADD CONSTRAINT proposal_feedback_refinement_injections_disposition_check
        CHECK (
            (state IN ('queued', 'injected', 'withdrawn_by_author')
                AND accepted_disposition IS NULL
                AND accepted_revision_seq IS NULL
                AND accepted_reason IS NULL
                AND accepted_at IS NULL
                AND accepted_by_user_id IS NULL)
            OR (state = 'accepted'
                AND accepted_disposition = 'fixed_revision'
                AND accepted_revision_seq IS NOT NULL
                AND accepted_reason IS NULL
                AND accepted_at IS NOT NULL)
            OR (state = 'wont_fix'
                AND accepted_disposition = 'wont_fix'
                AND accepted_revision_seq IS NULL
                AND accepted_reason IS NOT NULL
                AND length(btrim(accepted_reason)) > 0
                AND accepted_at IS NOT NULL)
        );
