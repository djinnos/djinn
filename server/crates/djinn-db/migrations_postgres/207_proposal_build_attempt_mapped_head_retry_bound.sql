-- The direct-delivery reconciler parks after its bounded set of distinct
-- ledger-mapped stale heads. Keep this additive: 203 may already be applied.
ALTER TABLE proposal_build_attempts
    DROP CONSTRAINT proposal_build_attempts_park_reason_check,
    ADD CONSTRAINT proposal_build_attempts_park_reason_check CHECK (
        park_reason IS NULL OR park_reason IN (
            'branch_identity_mismatch',
            'proposal_pr_identity_mismatch',
            'unexpected_branch_head',
            'mapped_head_retry_bound',
            'delivery_conflict',
            'no_proposal_owner',
            'capability_unavailable',
            'epoch_disabled',
            'lease_lost'
        )
    );
