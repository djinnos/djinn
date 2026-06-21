-- Proposal feedback is now a single per-proposal thread; the per-block
-- anchoring (`target_section`) and its UI rail have been removed. Drop the
-- column so the feedback model has one source of truth.
ALTER TABLE proposal_feedback DROP COLUMN IF EXISTS target_section;
