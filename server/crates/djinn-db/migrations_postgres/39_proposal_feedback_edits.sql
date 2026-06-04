-- Phase 1, Step 3 — editable suggestions.
--
-- A feedback entry can now carry a concrete proposed change to the spec body
-- (an "edit suggestion", like a GitHub suggestion). Accepting it applies the
-- change — which appends a revision — and records the revision it landed in.
-- `proposed_body IS NULL` keeps a plain discussion/comment as before.

ALTER TABLE proposal_feedback ADD COLUMN proposed_body TEXT NULL;
ALTER TABLE proposal_feedback ADD COLUMN applied_revision_seq INT NULL;
