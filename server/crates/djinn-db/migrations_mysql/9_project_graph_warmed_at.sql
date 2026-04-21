-- Migration 9: `projects.graph_warmed_at` — signal that the canonical-graph
-- warmer has completed at least once for this project.
--
-- Populated by `RepoGraphCacheRepository::upsert`: every successful graph
-- cache write (in-process or K8sGraphWarmer Job) stamps the current UTC
-- timestamp as RFC3339 on the project row. An empty string means the warm
-- has never run (cold project or failing pipeline).
--
-- VARCHAR(64) RFC3339 matches the existing timestamp convention for this
-- schema (see migration 2's retrospective: DATETIME/TIMESTAMP columns don't
-- round-trip cleanly with sqlx when the repository reads them as String).
--
-- The coordinator's task dispatch loop uses this as a hard gate: it will
-- not dispatch any task for a project whose `image_status != 'ready'` or
-- whose `graph_warmed_at` is empty. This validates the end-to-end
-- devcontainer + warmer chain before any real work runs.

ALTER TABLE projects
    ADD COLUMN graph_warmed_at VARCHAR(64) NOT NULL DEFAULT '';
