-- Migration 14: cut over from the aider-style `repo_map_cache` (PageRank-
-- over-SCIP rendered text; never wired to a consumer in production) to a
-- commit-based file-coupling index derived from `git log`.
--
-- Commits are the ground-truth "changed together" signal. Store raw
-- per-commit per-file facts and compute aggregates (coupling, churn) as
-- queries so that policy knobs (big-commit filtering, decay, time windows)
-- become query parameters instead of schema migrations.

-- ── drop the orphan repo-map cache ─────────────────────────────────────
DROP TABLE IF EXISTS repo_map_cache;

-- ── commit_file_changes ────────────────────────────────────────────────
-- One row per (project, commit, file) touched.
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
    PRIMARY KEY (project_id, commit_sha, file_path)
);

CREATE INDEX commit_file_changes_idx_file ON commit_file_changes (project_id, file_path);
CREATE INDEX commit_file_changes_idx_committed_at ON commit_file_changes (project_id, committed_at);

-- ── coupling_cursor ────────────────────────────────────────────────────
-- Per-project high-water mark for the coupling ingest.
CREATE TABLE IF NOT EXISTS coupling_cursor (
    project_id       VARCHAR(36) NOT NULL PRIMARY KEY,
    last_indexed_sha VARCHAR(64) NOT NULL,
    last_updated_at  VARCHAR(64) NOT NULL
);
