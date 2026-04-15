-- V20260415000002__projects_installation_id.sql
--
-- SQLite counterpart of MySQL migration V4. Adds an optional installation_id
-- column to the `projects` table so GitHub-origin projects can record which
-- GitHub App installation grants access to the repo. Pre-existing rows leave
-- it NULL.

ALTER TABLE projects ADD COLUMN installation_id INTEGER;
