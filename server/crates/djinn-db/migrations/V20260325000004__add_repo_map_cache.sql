CREATE TABLE repo_map_cache (
    project_id        TEXT NOT NULL,
    project_path      TEXT NOT NULL,
    worktree_path     TEXT,
    commit_sha        TEXT NOT NULL,
    rendered_map      TEXT NOT NULL,
    token_estimate    INTEGER NOT NULL,
    included_entries  INTEGER NOT NULL,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (project_id, project_path, worktree_path, commit_sha)
);
