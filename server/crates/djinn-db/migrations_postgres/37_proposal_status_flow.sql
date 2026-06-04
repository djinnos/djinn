-- Phase 1, Step 1 — proposal status flow.
--
-- Renames the Phase-0 lifecycle to the review-gated flow:
--   draft → in_review → approved → building → done
-- with off-ramps rejected / archived / superseded. `shared` and `ready` are
-- folded into `in_review` and `approved` respectively. The column is a plain
-- VARCHAR so this is a pure data migration.

UPDATE proposals SET status = 'in_review' WHERE status = 'shared';
UPDATE proposals SET status = 'approved'  WHERE status = 'ready';
