-- Add recommended-model override columns to org AI policy.
--
-- Two new TEXT columns holding JSON arrays of fully-qualified "provider/model-id"
-- entries:
--   * `additional_recommended_model_ids` — admin-curated additions to the
--     baseline RECOMMENDED_MODELS set.
--   * `demoted_recommended_model_ids` — admin-curated removals from the
--     recommended set (demotion wins over addition at runtime).
--
-- Both default to '[]' so existing singleton rows are immediately valid without
-- a backfill UPDATE.

ALTER TABLE org_ai_policy
    ADD COLUMN IF NOT EXISTS additional_recommended_model_ids TEXT NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS demoted_recommended_model_ids    TEXT NOT NULL DEFAULT '[]';
