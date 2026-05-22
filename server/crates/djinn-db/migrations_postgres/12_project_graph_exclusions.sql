-- Migration 12: per-project graph-exclusion lists.
--
-- Pulse (and any MCP caller of `code_graph`) needs a way to suppress
-- project-specific noise from the cycles / orphans / ranked queries:
--   * `graph_excluded_paths` — JSON array of glob patterns applied to
--     every code_graph query's result set.
--   * `graph_orphan_ignore` — JSON array of exact file paths that the
--     Dead-code panel (orphans query) should silently drop.
--
-- Both columns default to the empty JSON array so no project starts
-- out filtered.

ALTER TABLE projects
    ADD COLUMN graph_excluded_paths JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN graph_orphan_ignore  JSONB NOT NULL DEFAULT '[]'::jsonb;
