-- Migration 14: cut over from the aider-style `repo_map_cache` (PageRank-
-- over-SCIP rendered text; never wired to a consumer in production) to a
-- commit-based file-coupling index derived from `git log`.
--
-- Rationale: commits are the ground-truth "changed together" signal, and
-- the SCIP-based repo map is dominated by generated/indexed noise for
-- agent-edit use cases. We store raw per-commit per-file facts and
-- compute aggregates (coupling, churn) as queries so that policy knobs
-- (big-commit filtering, decay, time windows) become query parameters
-- instead of schema migrations.
--
-- Cut-over: drop `repo_map_cache` in the same migration that creates the
-- new tables. The old repository has zero live readers (see
-- `docs/runbook/repo_map_sunset.md`), and the planner explicitly chose
-- a cut-over over a strangler-fig coexistence.

-- ── drop the orphan repo-map cache ─────────────────────────────────────
DROP TABLE IF EXISTS repo_map_cache;

-- ── commit_file_changes ────────────────────────────────────────────────
-- One row per (project, commit, file) touched. Renames keep the
-- post-rename path in `file_path` and the old path in `old_path`;
-- binary diffs set insertions/deletions to 0.
CREATE TABLE IF NOT EXISTS commit_file_changes (
    project_id    VARCHAR(36)  NOT NULL,
    commit_sha    VARCHAR(64)  NOT NULL,
    file_path     VARCHAR(512) NOT NULL,
    change_kind   VARCHAR(4)   NOT NULL,
    committed_at  VARCHAR(64)  NOT NULL,
    author_email  VARCHAR(255) NOT NULL,
    insertions    BIGINT       NOT NULL DEFAULT 0,
    deletions     BIGINT       NOT NULL DEFAULT 0,
    old_path      VARCHAR(512) NULL,
    PRIMARY KEY (project_id, commit_sha, file_path),
    KEY idx_file (project_id, file_path),
    KEY idx_committed_at (project_id, committed_at)
);

-- ── coupling_cursor ────────────────────────────────────────────────────
-- Per-project high-water mark for the coupling ingest. When present, we
-- only walk `cursor..HEAD` on the next warmer pass; absent rows trigger
-- a full-history ingest.
CREATE TABLE IF NOT EXISTS coupling_cursor (
    project_id       VARCHAR(36) NOT NULL PRIMARY KEY,
    last_indexed_sha VARCHAR(64) NOT NULL,
    last_updated_at  VARCHAR(64) NOT NULL
);
