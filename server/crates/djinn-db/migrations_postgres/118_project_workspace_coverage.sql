-- Per-(project, workspace, language) index-coverage accounting.
--
-- The code graph is best-effort: SCIP indexers fail per-workspace (the Ruby
-- indexer command is a known FIXME, Java/.NET are unverified, rust-analyzer /
-- scip-typescript can hit their wall-clock caps) and the warm succeeds with
-- whatever workspaces remain. `project_workspace_graph` records a bare
-- per-workspace `status`, but agents querying `code_graph` still get silent
-- false negatives (`dead_symbols` calls a symbol dead that is referenced from an
-- unindexed workspace, `impact` under-reports, `search` misses definitions) with
-- no signal anything is missing.
--
-- This table is the richer sibling of `project_workspace_graph`: one row per
-- (project, workspace, language) carrying the coverage outcome and its extent so
-- the `code_graph coverage` op, the per-response coverage advisory, and the UI
-- can name exactly which workspaces are not indexed. Timing already lives in
-- `scip_indexer_timing`; this adds OUTCOME + EXTENT. Rows are written on both the
-- success (partial-failure) and total-failure warm paths.
--
-- Written as a replace-set per warm (mirrors `project_workspace_graph`), so a
-- vanished workspace/language never leaves a ghost coverage row.
--
--   * `status` — coverage enum: one of
--       indexed | indexer_failed | timed_out | unsupported_language | excluded.
--   * `detail` — indexer exit detail (stderr tail / exit code / timeout reason).
--   * `workspace_root` — workspace root RELATIVE to the project root (empty for
--     the repo root). Lets `impact_check` map scope crates → workspaces to decide
--     whether an unindexed workspace actually intersects the analysed scope.
--   * `marker_evidence` — the marker file(s) whose presence caused this workspace
--     to be detected for this language (e.g. `Cargo.toml`, `tsconfig.json`).
--   * `discovered_files` / `indexed_files` — candidate source files found under
--     the workspace root vs. distinct files that made it into the graph. A gap
--     (discovered > 0, indexed = 0) is the honest "covered nothing" signal.
CREATE TABLE IF NOT EXISTS project_workspace_coverage (
    project_id       VARCHAR(36)   NOT NULL,
    workspace_slug   VARCHAR(255)  NOT NULL,
    language         VARCHAR(64)   NOT NULL,
    status           VARCHAR(32)   NOT NULL,
    detail           TEXT,
    workspace_root   VARCHAR(1024) NOT NULL DEFAULT '',
    marker_evidence  VARCHAR(255),
    discovered_files BIGINT,
    indexed_files    BIGINT,
    commit_sha       VARCHAR(64)   NOT NULL,
    warmed_at        VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (project_id, workspace_slug, language),
    CONSTRAINT fk_project_workspace_coverage_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS project_workspace_coverage_project
    ON project_workspace_coverage (project_id);
