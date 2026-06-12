-- Add the persistent inflight dispatch ledger columns to dispatch_state.
--
-- Proposal 8ipw / storage-primitives epic n6xw stores the in-flight
-- dispatch marker (creator user id and chosen model id) per task so the
-- coordinator can rebuild the in-memory `inflight_dispatches` ledger at
-- startup. With this ledger rehydrated, the per-user/per-model cap
-- overlay can account for dispatches that have been issued but are not
-- yet visible as `running` sessions in the DB, preventing cap overshoot
-- immediately after a coordinator restart.
--
-- Both columns are nullable: pre-existing rows and tasks that have
-- never been dispatched simply store NULL. Cleanup of stale inflight
-- markers is handled by the `last_dispatched_at` and dispatch_ready_tasks
-- flow; the missing rows after a successful dispatch are cleared via the
-- existing `clear()` path, which now also NULLs these two columns.
ALTER TABLE dispatch_state
    ADD COLUMN IF NOT EXISTS inflight_creator_user_id VARCHAR(255) NULL,
    ADD COLUMN IF NOT EXISTS inflight_model_id        VARCHAR(255) NULL;
