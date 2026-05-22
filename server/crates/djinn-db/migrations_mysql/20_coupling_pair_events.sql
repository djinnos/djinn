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

-- Phase 1 (Dolt → MySQL migration) schema change:
--
-- The natural composite identity `(project_id, file_a, file_b,
-- commit_sha)` under utf8mb4 (36 + 512 + 512 + 64) × 4 ≈ 4.5 KiB sums
-- past InnoDB's 3072-byte key limit, so MySQL 8.4 refuses to create a
-- PK / UNIQUE index over those columns (Dolt silently accepted it).
-- Mirroring the `repo_map_cache` pattern from migration 1, identity is
-- normalised into a SHA-256 hex digest column (`event_key`) the
-- application populates on insert. The application computes:
--     SHA-256(project_id || ':' || file_a || ':' || file_b || ':' || commit_sha)
-- and binds it as the first column. `ON DUPLICATE KEY UPDATE` keeps
-- the same idempotent semantics as before — the same natural tuple
-- hashes to the same `event_key`, so replays no-op exactly like the
-- old composite PK did.
--
-- The natural composite is demoted to a non-unique secondary index;
-- column prefixes keep it under the 3072-byte cap. The
-- `(file_a, file_b)` halves are not used as full equality probes (the
-- read path filters by `project_id`, then either ranges on
-- `committed_at` or aggregates), so the prefix truncation is harmless.
CREATE TABLE IF NOT EXISTS coupling_pair_events (
    -- SHA-256 hex digest of (project_id || ':' || file_a || ':' ||
    -- file_b || ':' || commit_sha). Application-populated. See
    -- `coupling_event_key()` in djinn_db / djinn_graph.
    event_key    CHAR(64)     NOT NULL PRIMARY KEY,
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
    -- Natural identity, now non-unique (uniqueness lives on event_key).
    -- Prefixed to fit under InnoDB's 3072-byte key limit; the prefixes
    -- are only used for indexed lookups by (project_id, file_a) /
    -- (project_id, file_a, file_b), and equality on full file_a /
    -- file_b is settled by the row payload after the seek.
    KEY idx_natural (project_id, file_a(191), file_b(191), commit_sha),
    -- Range scans for windowed queries (`WHERE project_id = ? AND
    -- committed_at >= ?`). `coupling_hubs` / `coupling_hotspots` /
    -- `coupling` all hit this path.
    KEY idx_recent (project_id, committed_at),
    -- Per-pair lookup for the future `coupling_evidence` op that
    -- returns the actual commits behind a pair.
    KEY idx_pair (project_id, file_a(191), file_b(191))
);

-- Note: we intentionally do NOT backfill from `commit_file_changes`
-- here. The Rust-side ingest path picks up the empty table on the
-- next warmer pass and rebuilds from existing rows — see
-- `djinn_graph::coupling_index::rebuild_pairs_from_changes`.
