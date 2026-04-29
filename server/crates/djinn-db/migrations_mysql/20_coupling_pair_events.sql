-- Migration 20: materialised file-coupling pair events.
--
-- Background: the `coupling_hubs` / `coupling_hotspots` queries used to
-- self-join `commit_file_changes` and filter big commits via a
-- correlated `IN (SELECT … HAVING COUNT(*) <= ?)` subquery. Dolt's
-- query planner does not push the IN cap before materialising the
-- self-join, so on any project with even a handful of "fat" commits
-- (initial imports, codemods, lockfile refreshes) the join blew up
-- into a multi-million-row cartesian and never returned. The chat
-- handler had no per-tool timeout, so a single coupling call wedged
-- the whole stream forever.
--
-- Fix: maintain a materialised table of `(file_a, file_b, commit_sha)`
-- pair events. The ingest path filters out big commits (>15 files) at
-- write time — the cap that used to be a query-time IN filter is now
-- a write-time policy, so the slow shape never executes. Coupling
-- queries collapse to indexed range scans + Rust-side aggregation,
-- which is sub-100ms even on long histories.
--
-- The schema is per-commit (not bucketed by month/day) so we can apply
-- arbitrary time windows AND exponential decay weighting at query
-- time without rebuilds. Cardinality stays small in practice — a
-- pair that co-edits 100 times over two years is 100 rows, and the
-- typical service has ~25-50K active pairs total.

CREATE TABLE IF NOT EXISTS coupling_pair_events (
    project_id   VARCHAR(36)  NOT NULL,
    -- file_a < file_b is enforced at ingest; this lets every ordered
    -- pair appear exactly once and lets us GROUP BY (a, b) cleanly.
    file_a       VARCHAR(512) NOT NULL,
    file_b       VARCHAR(512) NOT NULL,
    commit_sha   VARCHAR(64)  NOT NULL,
    -- ISO-8601 string, matches `commit_file_changes.committed_at`.
    -- Lexical compare matches chronological order, so range queries
    -- (`WHERE committed_at >= ?`) work without converting types.
    committed_at VARCHAR(64)  NOT NULL,
    -- (project, file_a, file_b, commit_sha) is naturally unique;
    -- making it the PK gives idempotent re-ingest (UPSERT no-ops on
    -- replays) and lets pair-lookup queries seek directly.
    PRIMARY KEY (project_id, file_a, file_b, commit_sha),
    -- Range scans for windowed queries (`WHERE project_id = ? AND
    -- committed_at >= ?`). `coupling_hubs` / `coupling_hotspots` /
    -- `coupling` all hit this path.
    KEY idx_recent (project_id, committed_at),
    -- Per-pair lookup for the future `coupling_evidence` op that
    -- returns the actual commits behind a pair.
    KEY idx_pair (project_id, file_a, file_b)
);

-- Note: we intentionally do NOT backfill from `commit_file_changes`
-- here. The Rust-side ingest path picks up the empty table on the
-- next warmer pass and rebuilds from existing rows — see
-- `djinn_graph::coupling_index::rebuild_pairs_from_changes`.
