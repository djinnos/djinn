-- Migration 7: Add projects.stack column for Phase 3 stack detection.
--
-- Populated by the mirror-fetcher hook after each successful fetch
-- (see `djinn_stack::detect`). Consumers: the `get_project_stack` MCP
-- tool (Phase 3 PR 2), the image-controller devcontainer-hash path
-- (Phase 3 PR 5), and the UI devcontainer onboarding banner
-- (Phase 3 PR 6).
--
-- Default `'{}'` is an empty JSON object; readers treat it as "no
-- detection yet" and the first mirror-fetcher tick overwrites it.

ALTER TABLE projects
    ADD COLUMN stack LONGTEXT NOT NULL DEFAULT ('{}');
