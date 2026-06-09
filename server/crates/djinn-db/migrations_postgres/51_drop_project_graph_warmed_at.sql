-- Drop the legacy project-level graph freshness scalar.
--
-- Consumers now derive graph freshness from `project_workspace_graph` first and
-- `repo_graph_cache` second. Code-less projects record a synthetic workspace
-- freshness row instead of writing to `projects`.
ALTER TABLE projects
    DROP COLUMN IF EXISTS graph_warmed_at;
