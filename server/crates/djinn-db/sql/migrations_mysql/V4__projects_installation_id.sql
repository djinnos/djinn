-- V4__projects_installation_id.sql
--
-- Migration 4: cache the GitHub **App installation id** on each project row.
--
-- Before this migration the push/PR-create path had to discover an installation
-- dynamically by walking the authenticated user's installations and listing
-- each installation's repos until `owner/repo` matched. That dance was both
-- slow (N API calls per push) and required a user token — leaking the legacy
-- OAuth user flow into every push.
--
-- Caching the installation id at `project_add_from_github` time lets the agent
-- push as `djinn-bot[bot]` using only App-level credentials (no user token
-- required). Pre-Migration-2 host-path projects and any Migration-2 rows
-- written before this column existed leave it NULL; callers must fail fast
-- with a clear error rather than falling back to the old user-token search.

ALTER TABLE projects
    ADD COLUMN installation_id BIGINT NULL;
