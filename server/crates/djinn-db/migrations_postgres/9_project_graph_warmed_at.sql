-- Migration 9: `projects.graph_warmed_at` — signal that the canonical-graph
-- warmer has completed at least once for this project.
--
-- Populated by `RepoGraphCacheRepository::upsert`: every successful graph
-- cache write stamps the current UTC timestamp as RFC3339 on the project row.
-- An empty string means the warm has never run (cold project or failing
-- pipeline).

ALTER TABLE projects
    ADD COLUMN graph_warmed_at VARCHAR(64) NOT NULL DEFAULT '';
