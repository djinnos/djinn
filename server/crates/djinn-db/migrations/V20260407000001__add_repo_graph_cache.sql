-- ADR-050 §3 Chunk C: per-commit canonical SCIP graph cache.
--
-- Stores the serialized RepoDependencyGraph keyed by (project_id,
-- commit_sha).  This is a server-wide cache (no worktree dimension) — under
-- ADR-050 the graph is built once per `origin/main` commit by
-- `ensure_canonical_graph` and reused by every architect/chat session and
-- every worker dispatch until `origin/main` advances.
CREATE TABLE IF NOT EXISTS repo_graph_cache (
    project_id TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    graph_blob BLOB NOT NULL,
    built_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (project_id, commit_sha)
);
