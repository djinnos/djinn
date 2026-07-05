-- Migration 95: Additive liveness classifier evidence, stable outcomes,
-- and claim-extension metadata for proposal `twis` / epic `5ric`.
--
-- Note: this migration is V95, not V94, because V94 (`task_attempts`) was
-- merged into main after this task started. Both migrations live alongside
-- each other and apply cleanly on a fresh database via sqlx::migrate!.
--
-- This is a foundation-only, additive schema: no existing objects are dropped
-- or renamed, and all new data is nullable so old rows remain readable
-- without a backfill.  Consumers in later epics (`vbgl`, `jk7v`, `ptvg`)
-- will read these columns/tables and wire mutating behavior.
--
-- Design choices:
--   * TEXT with CHECK constraints instead of enum rewrites — avoids locking
--     the existing sessions/task_runs rows and keeps old rows readable.
--   * JSONB evidence columns store machine-readable snapshots of the signals
--     that produced a verdict (pod phase, claim TTL, hard runtime, etc.).
--   * A dedicated `liveness_evidence` append-only table holds the full
--     classifier snapshot per classification pass, with an FK to sessions and
--     optional task_id / task_run_id so a session can be classified in the
--     context of its run or task.
--   * `task_runs` gets stable denormalized outcome kind/reason columns so
--     the most recent liveness outcome for a run is queryable without a
--     join.
--   * A `claim_extensions` table records each claim-extension decision so
--     later consumers can enforce budgets and audit slow-session handling.
--
-- Verdict values (stable, machine-readable):
--   live, slow, dead, protocol_violation
--
-- Outcome kind values (stable, machine-readable):
--   success, crash, timeout, dead_reclaimed, protocol_violation, kill_noop,
--   slow_extended
--
-- Reason values (stable, machine-readable):
--   clean_exit_nonterminal, nonzero_exit_nonterminal, hard_runtime_exceeded,
--   slow_extension_budget_exhausted

-- ── sessions: latest classifier verdict snapshot ───────────────────────────
-- These columns cache the most recent classifier result for a session so
-- queries that list active sessions do not need to join the full evidence
-- table. They are all nullable; old rows simply read as NULL.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS liveness_verdict TEXT NULL
        CONSTRAINT sessions_liveness_verdict_check
            CHECK (liveness_verdict IS NULL OR liveness_verdict IN
                   ('live', 'slow', 'dead', 'protocol_violation')),
    ADD COLUMN IF NOT EXISTS liveness_outcome_kind TEXT NULL
        CONSTRAINT sessions_liveness_outcome_kind_check
            CHECK (liveness_outcome_kind IS NULL OR liveness_outcome_kind IN
                   ('success', 'crash', 'timeout', 'dead_reclaimed', 'protocol_violation', 'kill_noop', 'slow_extended')),
    ADD COLUMN IF NOT EXISTS liveness_outcome_reason TEXT NULL
        CONSTRAINT sessions_liveness_outcome_reason_check
            CHECK (liveness_outcome_reason IS NULL OR liveness_outcome_reason IN
                   ('clean_exit_nonterminal', 'nonzero_exit_nonterminal', 'hard_runtime_exceeded', 'slow_extension_budget_exhausted')),
    ADD COLUMN IF NOT EXISTS liveness_evidence JSONB NULL
        DEFAULT NULL;

-- Index for coordinator liveness sweeps: active sessions by latest verdict.
CREATE INDEX IF NOT EXISTS idx_sessions_liveness_verdict
    ON sessions(liveness_verdict)
    WHERE liveness_verdict IS NOT NULL;

-- ── task_runs: stable liveness outcome kind/reason cache ───────────────────
-- Mirrors the outcome columns on sessions, but at the run level. This is the
-- natural granularity for crash/timeout/dead_reclaimed outcomes. All columns
-- are nullable; old runs without liveness data remain readable.
ALTER TABLE task_runs
    ADD COLUMN IF NOT EXISTS liveness_outcome_kind TEXT NULL
        CONSTRAINT task_runs_liveness_outcome_kind_check
            CHECK (liveness_outcome_kind IS NULL OR liveness_outcome_kind IN
                   ('success', 'crash', 'timeout', 'dead_reclaimed', 'protocol_violation', 'kill_noop', 'slow_extended')),
    ADD COLUMN IF NOT EXISTS liveness_outcome_reason TEXT NULL
        CONSTRAINT task_runs_liveness_outcome_reason_check
            CHECK (liveness_outcome_reason IS NULL OR liveness_outcome_reason IN
                   ('clean_exit_nonterminal', 'nonzero_exit_nonterminal', 'hard_runtime_exceeded', 'slow_extension_budget_exhausted')),
    ADD COLUMN IF NOT EXISTS liveness_evidence JSONB NULL
        DEFAULT NULL;

-- Index for reaper/board-health queries: runs by outcome kind.
CREATE INDEX IF NOT EXISTS idx_task_runs_liveness_outcome_kind
    ON task_runs(liveness_outcome_kind)
    WHERE liveness_outcome_kind IS NOT NULL;

-- ── liveness_evidence: append-only classifier snapshots ──────────────────────
-- One row per classification pass. The JSONB evidence column is the canonical
-- structured record of the signals that produced the verdict. It is not
-- authoritative for task state: a terminal task always wins over any liveness
-- row, and the classifier can record a `noop` / idempotent outcome.
CREATE TABLE IF NOT EXISTS liveness_evidence (
    id                  VARCHAR(36)   NOT NULL PRIMARY KEY,
    session_id          VARCHAR(36)   NOT NULL,
    task_id             VARCHAR(36)   NULL,
    task_run_id         VARCHAR(36)   NULL,
    verdict             VARCHAR(32)   NOT NULL,
    outcome_kind        VARCHAR(32)   NULL,
    outcome_reason      VARCHAR(64)   NULL,
    evidence            JSONB         NOT NULL DEFAULT '{}'::jsonb,
    created_at          VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT liveness_evidence_verdict_check
        CHECK (verdict IN ('live', 'slow', 'dead', 'protocol_violation')),
    CONSTRAINT liveness_evidence_outcome_kind_check
        CHECK (outcome_kind IS NULL OR outcome_kind IN
               ('success', 'crash', 'timeout', 'dead_reclaimed', 'protocol_violation', 'kill_noop', 'slow_extended')),
    CONSTRAINT liveness_evidence_outcome_reason_check
        CHECK (outcome_reason IS NULL OR outcome_reason IN
               ('clean_exit_nonterminal', 'nonzero_exit_nonterminal', 'hard_runtime_exceeded', 'slow_extension_budget_exhausted')),
    CONSTRAINT fk_liveness_evidence_session
        FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    CONSTRAINT fk_liveness_evidence_task
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE SET NULL,
    CONSTRAINT fk_liveness_evidence_task_run
        FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE SET NULL
);

-- Query paths:
--   1. Latest evidence per session (used by reaper/board-health).
--   2. Evidence per task or task_run (used by diagnostics / audit).
--   3. Evidence by verdict for sweeps.
CREATE INDEX IF NOT EXISTS idx_liveness_evidence_session_created
    ON liveness_evidence(session_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_liveness_evidence_task_run_created
    ON liveness_evidence(task_run_id, created_at DESC)
    WHERE task_run_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_liveness_evidence_task_created
    ON liveness_evidence(task_id, created_at DESC)
    WHERE task_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_liveness_evidence_verdict
    ON liveness_evidence(verdict);

-- ── claim_extensions: slow-session claim extension audit log ───────────────
-- One row per claim-extension event. The `triggering_verdict` records the
-- liveness evidence that authorized the extension, and `metadata` carries
-- budget/lane information for later consumers. Old sessions simply have no
-- rows here; this table is not backfilled.
CREATE TABLE IF NOT EXISTS claim_extensions (
    id                      VARCHAR(36)  NOT NULL PRIMARY KEY,
    session_id              VARCHAR(36)  NOT NULL,
    task_run_id             VARCHAR(36)  NULL,
    project_id              VARCHAR(36)  NOT NULL,
    -- The liveness evidence row that authorized this extension, if known.
    liveness_evidence_id    VARCHAR(36)  NULL,
    -- Was the extension actually granted? FALSE means the extension was
    -- evaluated but denied (e.g. budget exhausted). Both outcomes are recorded
    -- so budget-accounting and diagnostics can see the full history.
    granted                 BOOLEAN      NOT NULL DEFAULT TRUE,
    -- Slow-session extension budget counter before this extension. Later
    -- consumers will decrement / enforce the budget; this column records the
    -- value at the time of the decision.
    extension_budget_before INT          NOT NULL DEFAULT 0,
    extension_budget_after  INT          NOT NULL DEFAULT 0,
    -- ISO-8601 UTC timestamp of the extension decision.
    created_at              VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    -- Free-form JSONB metadata for downstream consumers: claim TTL, lane,
    -- trigger reason, etc. Keep it machine-readable (JSON object).
    metadata                JSONB        NOT NULL DEFAULT '{}'::jsonb,
    CONSTRAINT claim_extensions_liveness_evidence_fk
        FOREIGN KEY (liveness_evidence_id) REFERENCES liveness_evidence(id) ON DELETE SET NULL,
    CONSTRAINT fk_claim_extensions_session
        FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    CONSTRAINT fk_claim_extensions_task_run
        FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE SET NULL,
    CONSTRAINT fk_claim_extensions_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

-- Query paths:
--   1. Extension history per session (newest first).
--   2. Extension history per task run.
--   3. Extension history per project (for project-level budget accounting).
CREATE INDEX IF NOT EXISTS idx_claim_extensions_session_created
    ON claim_extensions(session_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_claim_extensions_task_run_created
    ON claim_extensions(task_run_id, created_at DESC)
    WHERE task_run_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_claim_extensions_project_created
    ON claim_extensions(project_id, created_at DESC);
