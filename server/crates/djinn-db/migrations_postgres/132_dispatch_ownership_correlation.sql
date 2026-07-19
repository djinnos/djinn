-- Migration 132: Durable coordinator-incarnation leases and dispatch
-- ownership/correlation columns (epic jy7g / proposal 9gg5).
--
-- This migration is purely additive:
--   * A new `coordinator_incarnations` table stores immutable random UUIDs and
--     their lease timestamps so a coordinator process can fence renewal to its
--     own exact incarnation.
--   * `task_attempts` gains nullable `dispatch_owner_incarnation_id` and
--     `dispatch_group_id` correlation columns.
--   * `task_runs` gains a nullable `dispatch_group_id` correlation column.
--   * Partial indexes back owner lookup and exact-group terminalization
--     queries without rewriting or rejecting existing rows.
--
-- Legacy rows that omit all new IDs remain NULL and fully usable; no heuristic
-- owner/group backfill is performed (sibling epic ars3 owns strike accounting,
-- and mixed-version behavior stays conservative).

-- ── Coordinator incarnation lease storage ────────────────────────────────────

CREATE TABLE IF NOT EXISTS coordinator_incarnations (
    -- Immutable random UUID generated once per coordinator process.
    id                VARCHAR(36)   NOT NULL PRIMARY KEY,

    -- When the incarnation was first registered (ISO-8601 UTC text).
    registered_at     VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),

    -- Last successful fenced renewal (ISO-8601 UTC text).  Updated only by the
    -- `WHERE id = $1` renewal so a different incarnation can never claim it.
    last_renewed_at   VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);

-- ── task_attempts correlation columns ────────────────────────────────────────

ALTER TABLE task_attempts
    ADD COLUMN IF NOT EXISTS dispatch_owner_incarnation_id VARCHAR(36) NULL,
    ADD COLUMN IF NOT EXISTS dispatch_group_id              VARCHAR(36) NULL;

-- ── task_runs correlation column ─────────────────────────────────────────────

ALTER TABLE task_runs
    ADD COLUMN IF NOT EXISTS dispatch_group_id VARCHAR(36) NULL;

-- ── Indexes ──────────────────────────────────────────────────────────────────

-- Owner-scoped lookup: attempts owned by a coordinator incarnation.  Partial on
-- non-NULL to avoid bloating the index with legacy NULL rows.
CREATE INDEX IF NOT EXISTS idx_task_attempts_dispatch_owner_incarnation
    ON task_attempts(dispatch_owner_incarnation_id)
    WHERE dispatch_owner_incarnation_id IS NOT NULL;

-- Exact-group terminalization: pending/submitted attempts in a dispatch group.
-- Partial on non-NULL group AND non-terminal outcome so the
-- `terminalize_dispatch_group` UPDATE (sibling task hhil) touches a tight set.
CREATE INDEX IF NOT EXISTS idx_task_attempts_dispatch_group
    ON task_attempts(dispatch_group_id)
    WHERE dispatch_group_id IS NOT NULL
      AND outcome IN ('pending', 'submitted');

-- Task-run group lookup: runs belonging to a dispatch group.
CREATE INDEX IF NOT EXISTS idx_task_runs_dispatch_group
    ON task_runs(dispatch_group_id)
    WHERE dispatch_group_id IS NOT NULL;
