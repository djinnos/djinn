-- Mirror migration 4's approach for the `agents` table: fill in JSON-shaped
-- defaults for `NOT NULL` JSONB columns that were originally written as bare
-- `NOT NULL`. The MySQL-era schema let bare `NOT NULL TEXT` columns coerce
-- missing inserts into the empty string; Postgres rejects them outright,
-- which surfaced as `null value in column "system_prompt_extensions" of
-- relation "agents"` whenever `seed_default_roles` ran on a newly-added
-- project (`INSERT INTO agents (id, project_id, name, base_role,
-- description, is_default) VALUES (…)` — system_prompt_extensions,
-- mcp_servers, and skills are all left to the column default).
--
-- Giving the columns explicit empty-collection defaults aligns them with
-- the other JSONB columns migrated in 4_default_json_columns.sql and lets
-- the seed path stay focused on the fields that actually vary per role.

ALTER TABLE agents
    ALTER COLUMN system_prompt_extensions SET DEFAULT '{}'::jsonb,
    ALTER COLUMN mcp_servers              SET DEFAULT '[]'::jsonb,
    ALTER COLUMN skills                   SET DEFAULT '[]'::jsonb;
