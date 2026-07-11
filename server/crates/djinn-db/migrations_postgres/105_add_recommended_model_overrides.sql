-- Add recommended-model override columns to org_ai_policy.
--
-- Admin-curated additions and demotions from the baseline RECOMMENDED_MODELS
-- set. Each entry is a fully qualified "provider/model-id". Demotion wins over
-- addition at runtime.

ALTER TABLE org_ai_policy
    ADD COLUMN IF NOT EXISTS additional_recommended_model_ids  TEXT NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS demoted_recommended_model_ids     TEXT NOT NULL DEFAULT '[]';
