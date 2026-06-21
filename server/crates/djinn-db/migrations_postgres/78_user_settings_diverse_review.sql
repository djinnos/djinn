-- Cross-model ("Thorough") review toggle for the reviewer lane.
--
-- Slice 3 of proposal p8py ("Model setup UX"). Adds a per-user `diverse_review`
-- boolean to `user_settings`. When ON (the default), a task dispatched to the
-- reviewer role prefers a model id DIFFERENT from the one that implemented the
-- task — walking the user's review-lane fallback list and taking the first
-- entry whose model id differs from the implementer's. If the viable list
-- collapses to the implementer's model, dispatch proceeds same-model rather
-- than blocking the task.
--
-- Defaults TRUE so existing users get cross-model review without a manual opt-in
-- (it is a no-op for anyone with fewer than 2 distinct model ids across their
-- implement+review lanes — the dispatch path falls back to same-model).

ALTER TABLE user_settings
    ADD COLUMN diverse_review BOOLEAN NOT NULL DEFAULT TRUE;
