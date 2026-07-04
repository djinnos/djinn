-- Migration 94: Durable task_attempts substrate (proposal jteh / epic u74z).
--
-- One logical row per dispatch attempt or guard-only deferred decision.  This
-- migration is additive only: no existing tables are dropped, renamed, or
-- backfilled.  Downstream epics (7w2i, w8a3, x4qa) consume this substrate for
-- park-rung, respawn guard, arbiter ledger, and prompt context wiring.
--
-- Lifecycle vocabulary
-- ~~~~~~~~~~~~~~~~~~~
-- Non-terminal outcomes: `pending`, `submitted`.
-- Terminal outcomes:     `completed`, `reopened`, `crashed`, `timed_out`,
--                        `cancelled`, `loop_guard_tripped`, `spawn_failed`,
--                        `deferred`, `adopted_pr`, `force_closed`, `handoff`.
--
-- An attempt row begins in `pending` (dispatch keyed) and advances to
-- `submitted` when the worker signals submission.  From `submitted` it moves to
-- exactly one terminal outcome.  The terminal transition is forward-only;
-- repository APIs must reject backward moves.  A guard-only row is inserted
-- directly with outcome `deferred` and `guard_decision = 'defer'`; it never
-- reaches `submitted`.
--
-- Idempotency / uniqueness
-- ~~~~~~~~~~~~~~~~~~~~~~~~
-- `dispatch_key` is a deterministic hash produced by the coordinator before
-- dispatch.  It is UNIQUE so that duplicate dispatches (e.g. a retried event)
-- insert at most one row.  `(task_id, attempt_seq)` is additionally UNIQUE
-- to enforce per-task monotonic sequencing — the repository must allocate the
-- next `attempt_seq` inside the same transaction as the INSERT.

CREATE TABLE IF NOT EXISTS task_attempts (
    id                VARCHAR(36)   NOT NULL PRIMARY KEY,

    -- Ownership
    task_id           VARCHAR(36)   NOT NULL,
    role              VARCHAR(64)   NOT NULL,

    -- Monotonic per-task sequence; starts at 1.
    attempt_seq       INTEGER       NOT NULL,

    -- Deterministic idempotency key; UNIQUE prevents duplicate dispatch rows.
    dispatch_key      VARCHAR(255)  NOT NULL,

    -- Nullable: guard-only rows have no backing session.
    session_id        VARCHAR(36)   NULL,

    -- Lifecycle state; CHECK enforces the vocabulary above.
    outcome           VARCHAR(64)   NOT NULL DEFAULT 'pending',

    -- Guard / defer metadata (nullable for non-guard rows).
    guard_decision    VARCHAR(32)   NULL,
    guard_reason      TEXT          NULL,

    -- Bounded text fields (redacted log tail for infra-death forensics).
    summary           VARCHAR(4000) NULL,
    summary_json      JSONB         NULL,
    log_tail          VARCHAR(8000) NULL,

    -- Durable refs populated during lifecycle advancement.
    checkpoint_ref    VARCHAR(255)  NULL,
    submit_ref        VARCHAR(255)  NULL,
    pr_url            VARCHAR(2048) NULL,
    mirror_head_sha   VARCHAR(64)   NULL,
    github_head_sha   VARCHAR(64)   NULL,

    -- Timestamps (ISO-8601 UTC text, same format as other tables).
    created_at        VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at        VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    submitted_at      VARCHAR(64)   NULL,
    terminal_at       VARCHAR(64)   NULL,

    -- ── Constraints ────────────────────────────────────────────────────────

    CONSTRAINT fk_task_attempts_task
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT fk_task_attempts_session
        FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE SET NULL,

    -- Outcome vocabulary: non-terminal + terminal.
    CONSTRAINT task_attempts_outcome_check
        CHECK (outcome IN (
            'pending', 'submitted',
            'completed', 'reopened', 'crashed', 'timed_out', 'cancelled',
            'loop_guard_tripped', 'spawn_failed', 'deferred',
            'adopted_pr', 'force_closed', 'handoff'
        )),

    -- Guard decision vocabulary (nullable for non-guard rows).
    CONSTRAINT task_attempts_guard_decision_check
        CHECK (guard_decision IS NULL OR guard_decision IN (
            'allow', 'defer', 'block'
        )),

    -- attempt_seq must be positive (1-based).
    CONSTRAINT task_attempts_attempt_seq_check
        CHECK (attempt_seq > 0),

    -- summary_json, when present, must be a JSON object.
    CONSTRAINT task_attempts_summary_json_check
        CHECK (summary_json IS NULL OR jsonb_typeof(summary_json) = 'object'),

    -- ── Uniqueness / idempotency ──────────────────────────────────────────

    -- Duplicate dispatch key → at most one row.
    CONSTRAINT task_attempts_dispatch_key_unique
        UNIQUE (dispatch_key),

    -- Per-task monotonic attempt sequence.
    CONSTRAINT task_attempts_task_id_attempt_seq_unique
        UNIQUE (task_id, attempt_seq)
);

-- ── Indexes ──────────────────────────────────────────────────────────────────

-- dispatch_key: already backed by UNIQUE constraint index (implicit B-tree).

-- Per-task newest-first ordering: primary prompt/history lookup.
CREATE INDEX idx_task_attempts_task_created
    ON task_attempts(task_id, created_at DESC);

-- Per-task role-filtered newest-first (prompt context assembly).
CREATE INDEX idx_task_attempts_task_role_created
    ON task_attempts(task_id, role, created_at DESC);

-- Active-attempt lookup: non-terminal outcomes by task (pending + submitted).
CREATE INDEX idx_task_attempts_task_outcome
    ON task_attempts(task_id, outcome)
    WHERE outcome IN ('pending', 'submitted');

-- Session-scoped lookup: find attempts that ran under a given session.
CREATE INDEX idx_task_attempts_session_id
    ON task_attempts(session_id)
    WHERE session_id IS NOT NULL;

-- Terminal-attempt ordering: recovery/arbiter queries that scan by terminal_at.
CREATE INDEX idx_task_attempts_task_terminal
    ON task_attempts(task_id, terminal_at DESC)
    WHERE terminal_at IS NOT NULL;
