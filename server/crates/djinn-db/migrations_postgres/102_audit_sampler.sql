-- Audit sampler: durable storage for the independent audit-sampler and
-- false-negative ledger (epic ihf1).
--
-- All tables are additive — no existing SQL objects are dropped or renamed.
-- The sampler reads only persisted facts + org sampling policy; no LLM or
-- risk scorer in the selection path.
--
-- Entities:
--   audit_merged_changes   — eligibility facts per merged change
--   audit_sample_policies  — org/project sample policy revisions
--   audit_sample_frames    — immutable sealed sample frame revisions
--   audit_selections       — selected draw / audit-item records
--   audit_outcomes         — false-negative ledger entries

-- ── Merged-change facts ───────────────────────────────────────────────────────
-- One row per merged change. Keyed by merge_commit_sha which is globally
-- unique. task_id is stored WITHOUT a FK so the ledger survives task-row
-- cleanup; the repository layer resolves it when needed.
CREATE TABLE IF NOT EXISTS audit_merged_changes (
    id                  VARCHAR(36)   NOT NULL PRIMARY KEY,
    project_id          VARCHAR(36)   NOT NULL,
    task_id             VARCHAR(36)   NULL,
    pr_number           BIGINT        NULL,
    head_sha            VARCHAR(64)   NULL,
    merge_commit_sha    VARCHAR(64)   NOT NULL,
    merged_at           VARCHAR(64)   NOT NULL,
    -- Gate / release provenance (opaque JSON blobs from tripwire payloads)
    gate_outcome        VARCHAR(64)   NOT NULL DEFAULT 'pass',
    gate_provenance     JSONB         NULL,
    release_provenance  JSONB         NULL,
    -- Stratum: 'unflagged_merged' or 'autonomous_release'
    stratum             VARCHAR(64)   NOT NULL DEFAULT 'unflagged_merged',
    -- Exclusion tracking
    excluded            BOOLEAN       NOT NULL DEFAULT FALSE,
    exclusion_reason    VARCHAR(512)  NULL,
    -- Timestamps
    created_at          VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at          VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT uq_audit_merged_changes_merge_sha UNIQUE (merge_commit_sha),
    CONSTRAINT fk_audit_merged_changes_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX idx_audit_merged_changes_project    ON audit_merged_changes(project_id);
CREATE INDEX idx_audit_merged_changes_task       ON audit_merged_changes(task_id);
CREATE INDEX idx_audit_merged_changes_merged_at  ON audit_merged_changes(merged_at);
CREATE INDEX idx_audit_merged_changes_stratum    ON audit_merged_changes(stratum);

-- ── Sample policies ──────────────────────────────────────────────────────────
-- Org/project-level sampling policy revisions. Each revision is immutable;
-- the coordinator bumps the revision when the operator changes rates or
-- stratum weights.
CREATE TABLE IF NOT EXISTS audit_sample_policies (
    id              VARCHAR(36)   NOT NULL PRIMARY KEY,
    project_id      VARCHAR(36)   NOT NULL,
    revision        INT           NOT NULL DEFAULT 1,
    policy_json     JSONB         NOT NULL,
    created_at      VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT uq_audit_sample_policies_project_rev UNIQUE (project_id, revision),
    CONSTRAINT fk_audit_sample_policies_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX idx_audit_sample_policies_project ON audit_sample_policies(project_id);

-- ── Sample frames ────────────────────────────────────────────────────────────
-- Immutable frame revisions. Late corrections create a new revision with an
-- audit event, never silent rewrites. The content_hash covers sorted eligible
-- ids so verifiers can recompute the hash.
CREATE TABLE IF NOT EXISTS audit_sample_frames (
    id                  VARCHAR(36)   NOT NULL PRIMARY KEY,
    project_id          VARCHAR(36)   NOT NULL,
    policy_id           VARCHAR(36)   NOT NULL,
    window_start        VARCHAR(64)   NOT NULL,
    window_end          VARCHAR(64)   NOT NULL,
    -- Revision counter within (project, window); starts at 1.
    revision            INT           NOT NULL DEFAULT 1,
    -- Sorted list of eligible merged_change ids (JSON array of strings).
    eligible_change_ids JSONB         NOT NULL DEFAULT '[]'::jsonb,
    -- SHA-256 hex of the sorted eligible ids for integrity verification.
    content_hash        CHAR(64)      NULL,
    -- Counts / reasons per exclusion category (JSON object).
    exclusion_counts    JSONB         NOT NULL DEFAULT '{}'::jsonb,
    exclusion_reasons   JSONB         NOT NULL DEFAULT '[]'::jsonb,
    -- Supersession chain: only the latest revision per window is active.
    superseded_by_id    VARCHAR(36)   NULL,
    sealed_at           VARCHAR(64)   NOT NULL,
    created_at          VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT uq_audit_sample_frames_proj_win_rev UNIQUE (project_id, window_start, window_end, revision),
    CONSTRAINT fk_audit_sample_frames_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_audit_sample_frames_policy
        FOREIGN KEY (policy_id) REFERENCES audit_sample_policies(id),
    CONSTRAINT fk_audit_sample_frames_superseded
        FOREIGN KEY (superseded_by_id) REFERENCES audit_sample_frames(id) ON DELETE SET NULL
);

CREATE INDEX idx_audit_sample_frames_project    ON audit_sample_frames(project_id);
CREATE INDEX idx_audit_sample_frames_policy     ON audit_sample_frames(policy_id);
CREATE INDEX idx_audit_sample_frames_window     ON audit_sample_frames(project_id, window_start, window_end);

-- ── Selections (draw / audit-item records) ───────────────────────────────────
-- Each row is one selected merged change within a sealed frame. The replay
-- algorithm + seed fields allow any verifier to re-derive the same draw.
CREATE TABLE IF NOT EXISTS audit_selections (
    id                  VARCHAR(36)   NOT NULL PRIMARY KEY,
    frame_id            VARCHAR(36)   NOT NULL,
    merged_change_id    VARCHAR(36)   NOT NULL,
    stratum             VARCHAR(64)   NOT NULL,
    selected_position   INT           NOT NULL,
    -- Replayability fields
    algorithm           VARCHAR(64)   NOT NULL DEFAULT 'hmac-sha256-counter-v1',
    seed_commitment     CHAR(64)      NOT NULL,
    seed_reveal         CHAR(64)      NULL,
    -- Opaque replay data (counter sequence, HMAC tags, etc.)
    replay_data         JSONB         NOT NULL DEFAULT '{}'::jsonb,
    -- Linked audit task (created as an ordinary review task for the operator)
    audit_task_id       VARCHAR(36)   NULL,
    created_at          VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_audit_selections_frame
        FOREIGN KEY (frame_id) REFERENCES audit_sample_frames(id),
    CONSTRAINT fk_audit_selections_merged_change
        FOREIGN KEY (merged_change_id) REFERENCES audit_merged_changes(id),
    -- task FK intentionally omitted to preserve selection rows if the
    -- audit task is later deleted
    CONSTRAINT uq_audit_selections_frame_pos UNIQUE (frame_id, selected_position)
);

CREATE INDEX idx_audit_selections_frame         ON audit_selections(frame_id);
CREATE INDEX idx_audit_selections_merged_change ON audit_selections(merged_change_id);
CREATE INDEX idx_audit_selections_task          ON audit_selections(audit_task_id);

-- ── Audit outcomes (false-negative ledger) ───────────────────────────────────
-- One outcome per selection. Accumulates ground truth for false-negative
-- rate estimation per stratum.
CREATE TABLE IF NOT EXISTS audit_outcomes (
    id                  VARCHAR(36)   NOT NULL PRIMARY KEY,
    selection_id        VARCHAR(36)   NOT NULL,
    -- Outcome: 'clean' or 'miss'
    outcome             VARCHAR(64)   NOT NULL,
    -- Miss metadata (nullable when outcome = 'clean')
    miss_category       VARCHAR(128)  NULL,
    miss_severity       VARCHAR(64)   NULL,
    requires_rule_update BOOLEAN      NOT NULL DEFAULT FALSE,
    notes               TEXT          NULL,
    recorded_at         VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    created_at          VARCHAR(64)   NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT uq_audit_outcomes_selection UNIQUE (selection_id),
    CONSTRAINT fk_audit_outcomes_selection
        FOREIGN KEY (selection_id) REFERENCES audit_selections(id)
);

CREATE INDEX idx_audit_outcomes_outcome     ON audit_outcomes(outcome);
CREATE INDEX idx_audit_outcomes_recorded_at ON audit_outcomes(recorded_at);
