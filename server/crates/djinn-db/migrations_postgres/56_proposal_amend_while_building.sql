-- Proposal amend-while-building drift tracking.
--
-- Phase 1 only records that an in-flight build is behind the latest proposal
-- revision; repository behavior changes and reconciler dispatch land in later
-- tasks. Existing building proposals are backfilled to drift = 0 so they do
-- not appear stale immediately after the migration.
ALTER TABLE proposals ADD COLUMN last_reconciled_revision_seq INT NULL;
ALTER TABLE proposals ADD COLUMN reconciled_at TIMESTAMP NULL;
ALTER TABLE proposals ADD COLUMN pending_reconcile BOOLEAN NOT NULL DEFAULT false;

UPDATE proposals
SET last_reconciled_revision_seq = latest_revision_seq,
    pending_reconcile = false
WHERE status = 'building';
