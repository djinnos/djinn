-- 109_extension_load_diagnostics.sql
--
-- V1 durable store for extension-load diagnostics (epic wvg5 / proposal 0h1s).
--
-- Each row records one diagnostic identity shared by all projections of a
-- single load event within a load attempt. Rows are deduplicated within the
-- same attempt on the project/task/session/attempt + source/phase/remedy/
-- summary-fingerprint tuple; later attempts create new identities.
--
-- Association semantics:
--   * doctor-only: task_id IS NULL AND session_id IS NULL. Lifetime follows
--     the owning project (project-audit retention).
--   * session-associated: session_id IS NOT NULL. An optional task_id may be
--     present. The owning session deletion cascades to these rows; task
--     deletion clears task_id but leaves the row.
--   * task_id without session_id is forbidden: a row cannot be owned by a
--     task without also being tied to the session that produced it.
--
-- Indexes:
--   * project_id, session_id, task_id, load_attempt_id — scoped reads
--   * (project_id, severity, source_kind, source_key, phase, id) — stable
--     deterministic ordering: error first, then source kind/key, phase, id
--   * NULLS-NOT-DISTINCT unique index on the dedupe tuple so that doctor-only
--     and session-associated rows with NULL task_id/session_id collide correctly.

CREATE TABLE IF NOT EXISTS extension_load_diagnostics (
    id                    VARCHAR(36)   NOT NULL PRIMARY KEY,
    project_id            VARCHAR(36)   NOT NULL,
    task_id               VARCHAR(36)   NULL,
    session_id            VARCHAR(36)   NULL,
    load_attempt_id       VARCHAR(36)   NOT NULL,
    schema_version        SMALLINT      NOT NULL DEFAULT 1,
    source_kind           VARCHAR(64)   NOT NULL,
    source_key            VARCHAR(512)  NOT NULL,
    phase                 VARCHAR(64)   NOT NULL,
    severity              VARCHAR(16)   NOT NULL,
    summary               VARCHAR(512)  NOT NULL,
    summary_fingerprint   VARCHAR(64)   NOT NULL,
    remedy_code           VARCHAR(64)   NOT NULL,
    remedy                VARCHAR(1024) NOT NULL,
    occurrence_count      INT           NOT NULL,
    first_seen_at         VARCHAR(64)   NOT NULL,
    last_seen_at          VARCHAR(64)   NOT NULL,
    created_at            VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT chk_extension_load_diagnostics_schema_version
        CHECK (schema_version = 1),
    CONSTRAINT chk_extension_load_diagnostics_association
        CHECK (task_id IS NULL OR session_id IS NOT NULL),
    CONSTRAINT chk_extension_load_diagnostics_severity
        CHECK (severity IN ('warning', 'error')),
    CONSTRAINT chk_extension_load_diagnostics_source_kind
        CHECK (source_kind IN ('project_mcp', 'project_skill')),
    CONSTRAINT chk_extension_load_diagnostics_phase
        CHECK (phase IN ('placeholder_resolution', 'process_start', 'transport', 'handshake', 'tools_list', 'frontmatter', 'missing_file', 'manifest_drift')),
    CONSTRAINT chk_extension_load_diagnostics_remedy_code
        CHECK (remedy_code IN ('check_placeholder', 'check_command', 'check_transport', 'check_server', 'check_skill_frontmatter', 'restore_skill_file', 'update_skill_manifest')),
    CONSTRAINT chk_extension_load_diagnostics_occurrence_count
        CHECK (occurrence_count > 0),
    CONSTRAINT fk_extension_load_diagnostics_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_extension_load_diagnostics_task
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE SET NULL,
    CONSTRAINT fk_extension_load_diagnostics_session
        FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_extension_load_diagnostics_project_id
    ON extension_load_diagnostics (project_id);

CREATE INDEX IF NOT EXISTS idx_extension_load_diagnostics_session_id
    ON extension_load_diagnostics (session_id);

CREATE INDEX IF NOT EXISTS idx_extension_load_diagnostics_task_id
    ON extension_load_diagnostics (task_id);

CREATE INDEX IF NOT EXISTS idx_extension_load_diagnostics_load_attempt_id
    ON extension_load_diagnostics (load_attempt_id);

-- Deterministic ordering: error first (alphabetically 'error' < 'warning'),
-- then source kind/key, phase, and finally the stable diagnostic id.
CREATE INDEX IF NOT EXISTS idx_extension_load_diagnostics_order
    ON extension_load_diagnostics (project_id, severity, source_kind, source_key, phase, id);

-- Same-attempt dedupe. NULLS NOT DISTINCT is required because task_id and
-- session_id are nullable and doctor-only rows must collide on the remaining
-- columns rather than being treated as distinct by the default NULL-unique
-- behavior.
CREATE UNIQUE INDEX IF NOT EXISTS uq_extension_load_diagnostics_dedupe
    ON extension_load_diagnostics (project_id, task_id, session_id, load_attempt_id, source_kind, source_key, phase, remedy_code, summary_fingerprint)
    NULLS NOT DISTINCT;
