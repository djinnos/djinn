-- Per-(project, workspace, indexer) SCIP indexer timing feedback.
--
-- Each warm run records the wall-clock elapsed and outcome of every SCIP
-- indexer invocation so the NEXT warm can size its per-invocation timeout to
-- observed reality instead of a hand-tuned static cap. The static cap history
-- (120 -> 600 -> 1200s, bumped by hand each time a heavy workspace like the
-- `server` Rust crate timed out and silently dropped from the graph) shows a
-- fixed cap never fits every workspace. This table feeds the adaptive budget
-- model in `djinn-graph::scip_indexer::budget`.
--
-- One row per key (latest observation + rolling evidence), not an unbounded
-- append log:
--   * `last_status` / `last_elapsed_ms` / `observed_at` — the most recent run.
--   * `success_elapsed_ms` — elapsed of the last SUCCESSFUL run. Feeds the
--     existing prior-timing hook (raise the budget for slow-but-passing runs,
--     still clamped by the per-indexer static max). Cleared when a later run of
--     any non-success outcome does NOT overwrite it; reset to the fresh value
--     on every success.
--   * `timed_out_elapsed_ms` — high-water elapsed of TIMED-OUT runs. A
--     workspace that keeps timing out grows headroom (next budget ~1.5x this
--     value) instead of retrying at the identical cap. Reset to NULL on the
--     next success, since a success proves the current cap was sufficient.
CREATE TABLE IF NOT EXISTS scip_indexer_timing (
    project_id           VARCHAR(36)  NOT NULL,
    workspace_slug       VARCHAR(255) NOT NULL,
    indexer              VARCHAR(64)  NOT NULL,
    last_status          VARCHAR(32)  NOT NULL,
    last_elapsed_ms      BIGINT       NOT NULL,
    success_elapsed_ms   BIGINT,
    timed_out_elapsed_ms BIGINT,
    observed_at          VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (project_id, workspace_slug, indexer),
    CONSTRAINT fk_scip_indexer_timing_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);
