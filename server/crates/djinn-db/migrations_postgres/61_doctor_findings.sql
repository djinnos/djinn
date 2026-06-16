-- 61_doctor_findings.sql
--
-- Doctor findings persistence (Doctor framework epic 08f0).
--
-- The Doctor framework runs structured `DoctorCheck`s and emits a stream of
-- `Finding`s. Each finding carries:
--   * severity:           one of `info` / `warn` / `critical`
--   * check_name:         the stable `DoctorCheck::name()` identifier that produced it
--   * entity_ids:         a JSON array of opaque entity identifiers the finding
--                         relates to (e.g. task ids, project ids, session ids).
--                         Always a JSON array (possibly empty) so report code
--                         can iterate without inspecting each row's shape.
--   * evidence:           structured check-specific evidence (query results,
--                         computed values, diagnostic snapshots). Free-form
--                         JSON so individual checks can attach whatever they
--                         need without schema churn.
--   * resolver_snapshot:  the resolver inputs and outputs the check used to
--                         observe state. Required by the shared-resolver fix
--                         invariant (Gas Town regression): the fix path must
--                         re-run the same resolver against these inputs rather
--                         than recomputing expected state from scratch.
--   * detail:             free-form human-readable text surfaced in reports.
--
-- `run_id` is opaque to the framework today (an MCP caller / leader-tick may
-- pass any identifier they want to group findings from one invocation), so
-- we keep it as a free-form VARCHAR rather than a typed reference. A
-- `report_id` of NULL represents an ad-hoc run.
--
-- The primary key is a UUIDv7 id (matches the rest of the schema). Indexes:
--   * `created_at` DESC — report listing: "recent findings", oldest-first /
--     newest-first scans.
--   * `check_name`      — per-check lookups (fix path, run-history).
--   * `(check_name, created_at DESC)` — combined report queries filtered by
--     a specific check.
--   * GIN on `entity_ids` — `entity_ids @> '["x"]'` lookups for
--     "findings touching entity X".

CREATE TABLE IF NOT EXISTS doctor_findings (
    id                VARCHAR(36)  NOT NULL PRIMARY KEY,
    -- Identifier for the run that produced this finding (an MCP call id, a
    -- leader-tick id, or NULL for ad-hoc single-finding inserts).
    run_id            VARCHAR(64)  NULL,
    -- The wall-clock timestamp the finding was recorded. Stored as a UTC
    -- ISO-8601 string (matches the convention used by the rest of djinn-db
    -- e.g. `dispatch_state.updated_at`) rather than TIMESTAMPTZ so callers
    -- can carry the same value into logs / APIs without timezone math.
    created_at        VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    check_name        VARCHAR(255) NOT NULL,
    -- info | warn | critical — constrained at the column level so a bad value
    -- surfaces as a database error rather than silently polluting reports.
    severity          VARCHAR(16)  NOT NULL,
    -- Opaque entity ids this finding relates to (tasks, projects, sessions…).
    -- Always a JSON array; an empty array is the "no specific entity" sentinel.
    entity_ids        JSONB        NOT NULL DEFAULT '[]'::jsonb,
    -- Structured check-specific evidence (query results, timestamps, computed
    -- values). The framework does not constrain its shape — each check owns
    -- the schema it produces.
    evidence          JSONB        NOT NULL DEFAULT '{}'::jsonb,
    -- Resolver inputs and outputs captured at check time. The fix path
    -- re-runs the same resolver against `resolver_snapshot->'inputs'` so it
    -- computes expected state from the same observation the check made.
    resolver_snapshot JSONB        NULL,
    -- Free-form human-readable text surfaced in reports.
    detail            TEXT         NULL,
    CONSTRAINT doctor_findings_severity_check
        CHECK (severity IN ('info', 'warn', 'critical'))
);

-- Recent-finding scans (e.g. "latest 50 findings for report X").
CREATE INDEX IF NOT EXISTS doctor_findings_created_at_idx
    ON doctor_findings (created_at DESC);

-- Per-check lookups for the fix path and run-history views.
CREATE INDEX IF NOT EXISTS doctor_findings_check_name_idx
    ON doctor_findings (check_name);

-- "Findings for check X, newest first" — common combined query.
CREATE INDEX IF NOT EXISTS doctor_findings_check_name_created_at_idx
    ON doctor_findings (check_name, created_at DESC);

-- "All findings touching entity Y" — `entity_ids @> '["..."]'`.
CREATE INDEX IF NOT EXISTS doctor_findings_entity_ids_gin_idx
    ON doctor_findings USING GIN (entity_ids jsonb_path_ops);
