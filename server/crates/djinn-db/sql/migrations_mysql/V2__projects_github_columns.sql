-- V2__projects_github_columns.sql
--
-- Migration 2: host-filesystem project selection → GitHub-repo selection.
--
-- Adds columns so a project can be defined entirely by a GitHub repo the
-- Djinn GitHub App can access. The server clones the repo into a
-- server-managed directory (`clone_path`) under `/root/.djinn/projects/...`
-- inside the container (persisted via the `${HOME}/.djinn` bind mount).
--
-- Backwards compatibility: the existing `path` column is kept (and kept
-- NOT NULL with the existing default) so the legacy host-path flow can
-- continue to insert rows. For GitHub-origin projects, `path` is populated
-- with the same value as `clone_path` so existing joins/queries keep
-- working without changes.

ALTER TABLE projects
    ADD COLUMN github_owner   VARCHAR(255) NULL,
    ADD COLUMN github_repo    VARCHAR(255) NULL,
    ADD COLUMN default_branch VARCHAR(255) NULL,
    ADD COLUMN clone_path     VARCHAR(512) NULL;

-- Uniqueness on owner/repo when both are set — prevents duplicate clones.
CREATE UNIQUE INDEX uq_projects_github_owner_repo
    ON projects (github_owner, github_repo);
