-- Migration 189: append-only note access ledger (proposal u46i AC6).
--
-- `notes.last_accessed` / `notes.access_count` are destructive, last-write-wins
-- scalars: they record *that* a note was touched, never *when*, *by which run*,
-- or *through which tool*. `P(memory_read | Injected)` cannot be derived from
-- them at all — there is no row per read to join against `retrieval_traces`.
--
-- This table is that missing join side. One row per note access, never updated
-- and never deleted by application code, tagged with the tool surface that
-- caused it so a `memory_search` result payload (which counts as an access per
-- ADR-054) is never mistaken for an explicit `memory_read` pull.
--
-- Design invariants:
--   * `note_id` deliberately has NO foreign key to `notes`: the ledger must
--     survive note deletion, exactly like `note_revision_events` (migration
--     109).
--   * `session_id` / `task_run_id` are NULLABLE. Not every access path carries
--     run attribution, and a row with no attribution is still evidence that the
--     read happened. Consumers MUST count unattributed rows explicitly rather
--     than silently dropping them from a denominator.
--   * `created_at` is an ISO-8601 UTC string in the same spelling as
--     `retrieval_traces.created_at`, so both sides of the join cast with
--     `::timestamptz` identically.
--
-- Additive only: a new table plus its indexes. Nothing existing is read,
-- rewritten, or constrained differently, so this is safe to apply to a live
-- database while older binaries are still serving.

CREATE TABLE IF NOT EXISTS note_access_events (
    id           VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id   VARCHAR(36) NOT NULL,
    note_id      VARCHAR(36) NOT NULL,
    -- Session that issued the access, when the calling surface knows it.
    session_id   VARCHAR(36) NULL,
    -- Task run that issued the access. Resolved from `sessions.task_run_id`
    -- when only a session is known; NULL when neither is available.
    task_run_id  VARCHAR(36) NULL,
    -- Tool surface that caused the access. `memory_read` is an explicit pull by
    -- the agent; `memory_search` is the ADR-054 result-set touch and is NOT a
    -- pull.
    source       VARCHAR(32) NOT NULL,
    created_at   VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),

    CONSTRAINT fk_note_access_events_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT chk_note_access_events_id_uuid
        CHECK (id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    CONSTRAINT chk_note_access_events_source
        CHECK (source IN ('memory_read', 'memory_search'))
);

-- (project_id, note_id, created_at desc) — "was this note read after time T?",
-- the exact shape of the injected-pull-rate correlated subquery.
CREATE INDEX IF NOT EXISTS idx_note_access_events_project_note_created
    ON note_access_events (project_id, note_id, created_at DESC);

-- (project_id, task_run_id) — run-scoped attribution lookups.
CREATE INDEX IF NOT EXISTS idx_note_access_events_project_task_run
    ON note_access_events (project_id, task_run_id);

-- (project_id, session_id) — session-scoped attribution lookups, used when a
-- trace and an access event share a session but no run id was resolvable.
CREATE INDEX IF NOT EXISTS idx_note_access_events_project_session
    ON note_access_events (project_id, session_id);
