-- 5wdh / iaho — retrieval trace storage for proposal ykkj phase 1.
--
-- One row per dispatch-injection event recording the memory candidates
-- considered, capped, and (for non-injected rows) skipped. Downstream
-- instrumentation (sibling epic mwtv) writes a row per injection; the
-- `memory_recall_trace` MCP tool (sibling epic liso) reads them.
--
-- Design invariants (see design/5wdh-roadmap):
--   * `candidates` is a JSONB array of candidate DTOs. Each candidate has a
--     `skipped_reason` that is one of the fixed vocabulary below, or NULL only
--     for injected candidates.
--   * `candidate_cap` documents the configured top-N cap applied before
--     persistence; `candidate_cap_exceeded` records whether the raw candidate
--     set exceeded the cap.
--   * `sampling_metadata` is optional JSONB for sampling decisions when
--     sampling is enabled (otherwise NULL).
--   * Repository write errors must be returnable/loggable by fail-open callers
--     without changing injection output.

CREATE TABLE IF NOT EXISTS retrieval_traces (
    id                          VARCHAR(36)   NOT NULL PRIMARY KEY,
    schema_version              INT           NOT NULL,
    project_id                  VARCHAR(36)   NOT NULL,
    session_id                  VARCHAR(36)   NULL,
    task_run_id                 VARCHAR(36)   NULL,
    task_id                     VARCHAR(36)   NULL,
    -- Entry point that triggered this retrieval/injection.
    -- Vocabulary: 'dispatch', 'jit_pitfalls', 'load_knowledge_context',
    -- 'format_knowledge_notes', 'memory_recall_trace'.
    entry_point                 VARCHAR(64)   NOT NULL,
    -- Opaque trigger context (task id, query, scope, etc.).
    trigger                     JSONB         NULL,
    -- JSONB array of candidate DTOs (see repository module for shape).
    candidates                  JSONB         NOT NULL DEFAULT '[]'::jsonb,
    -- Configured top-N candidate cap applied before persistence.
    -- Default trace candidate cap is 50 (see design/5wdh-roadmap).
    candidate_cap               INT           NOT NULL DEFAULT 50,
    -- Whether the raw candidate set exceeded candidate_cap.
    candidate_cap_exceeded      BOOLEAN       NOT NULL DEFAULT FALSE,
    -- Optional sampling metadata when sampling is enabled (otherwise NULL).
    sampling_metadata           JSONB         NULL,
    -- Per-phase durations in milliseconds (opaque JSONB object).
    durations_ms                JSONB         NOT NULL DEFAULT '{}'::jsonb,
    -- Estimated tokens injected into the downstream prompt.
    estimated_injected_tokens   INT           NOT NULL DEFAULT 0,
    created_at                  VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_retrieval_traces_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT chk_retrieval_traces_entry_point CHECK (entry_point IN (
        'dispatch',
        'jit_pitfalls',
        'load_knowledge_context',
        'format_knowledge_notes',
        'memory_recall_trace'
    ))
);

-- ── Required indexes ──────────────────────────────────────────────────────
-- (project_id, created_at desc) — recent traces for a project.
CREATE INDEX idx_retrieval_traces_project_created
    ON retrieval_traces (project_id, created_at DESC);

-- (project_id, session_id, created_at desc) — traces within a session.
CREATE INDEX idx_retrieval_traces_project_session_created
    ON retrieval_traces (project_id, session_id, created_at DESC);

-- (project_id, task_run_id, created_at desc) — traces for a task run.
CREATE INDEX idx_retrieval_traces_project_task_run_created
    ON retrieval_traces (project_id, task_run_id, created_at DESC);

-- (project_id, task_id, created_at desc) — traces for a task.
CREATE INDEX idx_retrieval_traces_project_task_created
    ON retrieval_traces (project_id, task_id, created_at DESC);

-- (project_id, entry_point, created_at desc) — traces by entry point.
CREATE INDEX idx_retrieval_traces_project_entry_point_created
    ON retrieval_traces (project_id, entry_point, created_at DESC);
