CREATE TABLE verification_cache (
    project_id  TEXT NOT NULL,
    commit_sha  TEXT NOT NULL,
    output      TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (project_id, commit_sha)
);
