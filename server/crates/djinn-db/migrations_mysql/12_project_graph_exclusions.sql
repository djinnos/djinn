-- Migration 12: per-project graph-exclusion lists.
--
-- Pulse (and any MCP caller of `code_graph`) needs a way to suppress
-- project-specific noise from the cycles / orphans / ranked queries:
--   * `graph_excluded_paths` — JSON array of glob patterns applied to
--     every code_graph query's result set. A node is dropped if its
--     file path, display name, or SCIP key matches any glob. Typical
--     entries: `"**/workspace-hack/**"`, `"**/test-support/**"`,
--     `"**/generated/**"`.
--   * `graph_orphan_ignore` — JSON array of exact file paths that the
--     Dead-code panel (orphans query) should silently drop. These are
--     files the user has reviewed and confirmed are intentionally
--     unused (fixture definitions, test helpers indexed by SCIP but
--     called only from `#[cfg(test)]`, etc.).
--
-- Both columns default to the empty JSON array so no project starts
-- out filtered. The `code_graph` MCP handler loads them via
-- `ProjectRepository::get_config` on every request; the filters are
-- applied post-cache, so changing them does not invalidate the
-- canonical graph warm.
--
-- `LONGTEXT NOT NULL DEFAULT ('[]')` mirrors migration 4's pattern for
-- JSON-shaped columns so existing inserts that omit these fields pick
-- up a valid empty array without the caller spelling it every time.

ALTER TABLE projects
    ADD COLUMN graph_excluded_paths LONGTEXT NOT NULL DEFAULT ('[]'),
    ADD COLUMN graph_orphan_ignore  LONGTEXT NOT NULL DEFAULT ('[]');
