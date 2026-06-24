-- Needs-evidence spike parking: when the Judge issues a needs-evidence verdict,
-- the proposal is parked (moved back to draft) while a research spike task is
-- open. Two columns track the parking state:
--
-- `linked_spike_task_id` — FK to the spike task that must close before the
--   proposal can resume refinement. ON DELETE SET NULL keeps the proposal row
--   safe if the task is hard-deleted.
--
-- `needs_evidence_claim` — the named load-bearing feasibility claim that the
--   Judge identified as needing research. Surfaced in tool responses so callers
--   see why the proposal is parked.

ALTER TABLE proposals ADD COLUMN linked_spike_task_id VARCHAR(36) NULL
    REFERENCES tasks(id) ON DELETE SET NULL;

ALTER TABLE proposals ADD COLUMN needs_evidence_claim TEXT NULL;
