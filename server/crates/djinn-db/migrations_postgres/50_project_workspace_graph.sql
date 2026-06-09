-- Per-workspace code graph freshness rows.
--
-- `repo_graph_cache` remains the merged graph blob keyed by (project_id, commit_sha).
-- This table records which workspace slugs have been warmed for a project so
-- consumers can reason about graph freshness without a project-level
-- freshness scalar.
CREATE TABLE IF NOT EXISTS project_workspace_graph (
    project_id     VARCHAR(36)  NOT NULL,
    workspace_slug VARCHAR(255) NOT NULL,
    commit_sha     VARCHAR(64)  NOT NULL,
    warmed_at      VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    status         VARCHAR(64)  NOT NULL,
    PRIMARY KEY (project_id, workspace_slug),
    CONSTRAINT fk_project_workspace_graph_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS project_workspace_graph_project_warmed_at
    ON project_workspace_graph (project_id, warmed_at DESC);
