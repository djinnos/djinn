-- Cross-model (\"Diverse\") refinement toggle for the proposal-refinement
-- roles (Advocate, Adversary, Judge).
--
-- Generalizes the existing `diverse_review` pattern. When ON (the default),
-- proposal-refinement roles (advocate, adversary, judge) prefer a model id
-- DIFFERENT from the primary task model — walking the relevant lane fallback
-- list and taking the first entry whose model id differs. If the viable list
-- collapses to the same model, dispatch proceeds same-model rather than
-- blocking the task.
--
-- Defaults TRUE so existing users get cross-model refinement without a manual
-- opt-in. It is a no-op when fewer than 2 distinct model ids are available —
-- the dispatch path falls back to same-model.

ALTER TABLE user_settings
    ADD COLUMN diverse_refinement BOOLEAN NOT NULL DEFAULT TRUE;
