-- Per-user model selection becomes per-ROLE "lanes".
--
-- Slice 2 of proposal p8py ("Model setup UX"). The flat per-user ordered model
-- list (`user_settings.models`, migration 31) is replaced by three lane-scoped
-- ordered lists keyed by the task's base role:
--
--   plan      → planner, architect, chat
--   implement → worker
--   review    → reviewer
--
-- (`lead` is a human role with no model; dispatch fallbacks treat it as `plan`.)
--
-- Stored as a single JSON object in the new `model_lanes` TEXT column:
--   { "plan": [...], "implement": [...], "review": [...] }
-- NULL / absent means "no explicit selection" → global fallback.
--
-- Cut-over migration (no compat shim): every existing flat selection is seeded
-- into all three lanes, so nobody loses their configuration. The legacy `models`
-- column is dropped — it is no longer read or written.

ALTER TABLE user_settings ADD COLUMN model_lanes TEXT NULL;

-- Seed the lanes from the existing flat list: the same ordered selection becomes
-- the default for plan, implement, and review. Only rows with a non-empty,
-- valid JSON array are migrated; everything else stays NULL (= global fallback).
UPDATE user_settings
SET model_lanes = json_build_object(
        'plan', models::json,
        'implement', models::json,
        'review', models::json
    )::text
WHERE models IS NOT NULL
  AND btrim(models) <> ''
  AND btrim(models) <> '[]'
  AND json_typeof(models::json) = 'array';

ALTER TABLE user_settings DROP COLUMN models;
