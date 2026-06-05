-- Collapse "comment" and "suggestion" into one feedback primitive.
--
-- The old model let any feedback carry a status (open|accepted|rejected) and an
-- inline `proposed_body` diff that "Accept" applied to the spec. In practice a
-- pasted human review was posted as status='open' with no proposed_body, so
-- "Accept" stamped a label and changed nothing — the author then hand-merged it
-- (see proposal v79a). Feedback is now plain discussion that cannot be applied
-- directly; changes flow through djinn in chat, which rewrites the spec via
-- proposal_update (appending a revision) and marks the feedback resolved.
--
--   resolved_at IS NULL                    → unresolved (shown, counts on rows)
--   resolved_at + resolved_revision_seq    → addressed in that revision
--   resolved_at + resolved_revision_seq NULL → dismissed without a spec change

ALTER TABLE proposal_feedback ADD COLUMN resolved_at          VARCHAR(64) NULL;
ALTER TABLE proposal_feedback ADD COLUMN resolved_revision_seq INT        NULL;
ALTER TABLE proposal_feedback ADD COLUMN resolved_by_user_id  VARCHAR(36) NULL;

-- Backfill: a terminal old status (accepted/rejected) becomes resolved at its
-- last-touched time; an applied edit carries its revision forward. 'open' and
-- plain comments (status NULL) stay unresolved.
UPDATE proposal_feedback
   SET resolved_at = updated_at,
       resolved_revision_seq = applied_revision_seq
 WHERE status IN ('accepted', 'rejected');

ALTER TABLE proposal_feedback DROP COLUMN status;
ALTER TABLE proposal_feedback DROP COLUMN proposed_body;
ALTER TABLE proposal_feedback DROP COLUMN applied_revision_seq;

-- The unresolved-feedback count drives a per-row badge in the proposals list;
-- a partial index keeps that correlated COUNT(*) subquery cheap.
CREATE INDEX proposal_feedback_unresolved
    ON proposal_feedback (proposal_id)
    WHERE resolved_at IS NULL;
