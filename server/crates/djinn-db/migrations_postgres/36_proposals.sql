-- Phase 0 — Global Proposals (collaboration-first).
--
-- A `proposal` is a global, project-INDEPENDENT artifact that people
-- (engineers and non-engineers) author, share, and refine collaboratively
-- before any execution work begins. Unlike epics it has NO project_id — it
-- targets zero, one, or many projects via `proposal_targets`, and that set is
-- editable over time (so a proposal can be re-targeted from one repo to another
-- without losing its identity, discussion, or history). Graduation of a
-- proposal into the project-scoped epic/task execution engine is intentionally
-- NOT modelled here — that is a later phase. This layer stands beside the epic
-- engine and touches none of it.

CREATE TABLE proposals (
    id                  VARCHAR(36)  NOT NULL PRIMARY KEY,
    -- Globally unique 4-char base36 short id (NOT per-project like epics).
    short_id            VARCHAR(32)  NOT NULL UNIQUE,
    title               VARCHAR(512) NOT NULL,
    -- Markdown spec body.
    body                TEXT         NOT NULL DEFAULT '',
    -- JSON array of acceptance-criteria strings (cast ::text in SELECTs).
    acceptance_criteria JSONB        NOT NULL DEFAULT '[]',
    -- Lifecycle: draft | shared | ready | archived | superseded.
    status              VARCHAR(64)  NOT NULL DEFAULT 'draft',
    -- Real user FK of the author (NULL for system/agent-authored). Bare column
    -- with no FK, mirroring epics.created_by_user_id.
    author_user_id      VARCHAR(36)  NULL,
    -- When this proposal is replaced by another, the successor's id.
    superseded_by       VARCHAR(36)  NULL REFERENCES proposals(id) ON DELETE SET NULL,
    created_at          VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at          VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    closed_at           VARCHAR(64)  NULL
);

CREATE INDEX proposals_status ON proposals(status);

-- M:N target projects. `role` distinguishes the primary write-target(s) from
-- reference-only repos. Editable at any time — this is the re-target capability.
CREATE TABLE proposal_targets (
    proposal_id VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    project_id  VARCHAR(36) NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    role        VARCHAR(32) NOT NULL DEFAULT 'primary',
    created_at  VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (proposal_id, project_id)
);

CREATE INDEX proposal_targets_project_id ON proposal_targets(project_id);

-- One unified feedback primitive: discussion AND suggestions live here.
--   status IS NULL          → plain discussion (a "comment")
--   status IN (open|accepted|rejected) → a trackable suggestion
-- AI authorship is built in from row one (author_kind='ai', author_model set)
-- so a future adversarial-review "war room" reuses this table verbatim.
CREATE TABLE proposal_feedback (
    id             VARCHAR(36)  NOT NULL PRIMARY KEY,
    proposal_id    VARCHAR(36)  NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    -- Threading/replies (no FK so a parent delete doesn't cascade replies away).
    parent_id      VARCHAR(36)  NULL,
    author_kind    VARCHAR(16)  NOT NULL DEFAULT 'user',
    author_user_id VARCHAR(36)  NULL,
    author_model   VARCHAR(128) NULL,
    body           TEXT         NOT NULL,
    target_section VARCHAR(128) NULL,
    status         VARCHAR(16)  NULL,
    created_at     VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at     VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);

CREATE INDEX proposal_feedback_proposal_id ON proposal_feedback(proposal_id);
