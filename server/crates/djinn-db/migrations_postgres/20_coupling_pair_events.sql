-- Migration 20: materialised file-coupling pair events.
--
-- Background: the `coupling_hubs` / `coupling_hotspots` queries used to
-- self-join `commit_file_changes` and filter big commits via a correlated
-- `IN (SELECT … HAVING COUNT(*) <= ?)` subquery. The ingest path now filters
-- out big commits (>15 files) at write time, so the slow shape never executes.
-- Coupling queries collapse to indexed range scans + Rust-side aggregation,
-- which is sub-100ms even on long histories.
--
-- Identity is normalised into a SHA-256 hex digest column (`event_key`)
-- the application populates on insert. The application computes:
--     SHA-256(project_id || ':' || file_a || ':' || file_b || ':' || commit_sha)
-- and binds it as the first column. `ON CONFLICT (event_key) DO UPDATE …`
-- keeps the same idempotent semantics — the same natural tuple hashes to
-- the same `event_key`, so replays no-op.
CREATE TABLE IF NOT EXISTS coupling_pair_events (
    -- SHA-256 hex digest of (project_id || ':' || file_a || ':' ||
    -- file_b || ':' || commit_sha). Application-populated.
    event_key    CHAR(64)     NOT NULL PRIMARY KEY,
    project_id   VARCHAR(36)  NOT NULL,
    -- file_a < file_b is enforced at ingest; this lets every ordered
    -- pair appear exactly once and lets us GROUP BY (a, b) cleanly.
    file_a       VARCHAR(512) NOT NULL,
    file_b       VARCHAR(512) NOT NULL,
    commit_sha   VARCHAR(64)  NOT NULL,
    -- ISO-8601 string, matches `commit_file_changes.committed_at`.
    -- Lexical compare matches chronological order, so range queries work
    -- without converting types.
    committed_at VARCHAR(64)  NOT NULL
);

-- Natural identity, now non-unique. Postgres has no 3072-byte cap so we
-- can index the full VARCHAR widths.
CREATE INDEX idx_coupling_pair_events_natural
    ON coupling_pair_events (project_id, file_a, file_b, commit_sha);

-- Range scans for windowed queries.
CREATE INDEX idx_coupling_pair_events_recent
    ON coupling_pair_events (project_id, committed_at);

-- Per-pair lookup for the future `coupling_evidence` op.
CREATE INDEX idx_coupling_pair_events_pair
    ON coupling_pair_events (project_id, file_a, file_b);
