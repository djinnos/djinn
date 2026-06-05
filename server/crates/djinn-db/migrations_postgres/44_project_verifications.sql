-- 44_project_verifications.sql
--
-- Extract verification rules OUT of `projects.environment_config` into their
-- own per-project table. Verification commands are a runtime concern — they
-- run *in* the task image but never affect image *content* — so coupling them
-- to the image config meant editing a `cargo test` → `cargo nextest` command
-- rebuilt the whole image (the rule was part of the hashed config, and
-- `set_environment_config` nulled `image_hash` on every write). Moving them out
-- means verification edits no longer trigger an image rebuild.
--
-- `EnvironmentConfig` is `#[serde(deny_unknown_fields)]`, so once the Rust
-- struct drops its `verification` field, any row that still carries a
-- `"verification"` key would FAIL to deserialize. This migration therefore
-- (1) creates the table, (2) backfills it from the existing nested key, and
-- (3) STRIPS the key from `environment_config` — all before the new binary
-- (which no longer knows the field) reads any row. Migrations run only in the
-- Helm pre-upgrade Job, ahead of the new app/worker pods, so the ordering holds.

CREATE TABLE IF NOT EXISTS project_verifications (
    project_id     VARCHAR(36) NOT NULL PRIMARY KEY,
    -- JSON array of {match_pattern, commands[]} — same shape as the former
    -- `environment_config.verification.rules`.
    rules          JSONB       NOT NULL DEFAULT '[]'::jsonb,
    schema_version INT         NOT NULL DEFAULT 1,
    -- "auto_detected" (seeded from stack) | "user_edited" (saved via MCP/UI).
    source         VARCHAR(32) NOT NULL DEFAULT 'auto_detected',
    created_at     VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at     VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_project_verifications_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

-- Backfill: copy each project's existing nested rules verbatim. Rows whose
-- config has no `verification` key (or no rules) get an empty array.
INSERT INTO project_verifications (project_id, rules, source)
SELECT
    id,
    COALESCE(environment_config -> 'verification' -> 'rules', '[]'::jsonb),
    'auto_detected'
FROM projects
ON CONFLICT (project_id) DO NOTHING;

-- Strip the now-extracted key so the verification-less struct deserializes
-- every row cleanly under deny_unknown_fields.
UPDATE projects
SET environment_config = environment_config - 'verification'
WHERE environment_config ? 'verification';
