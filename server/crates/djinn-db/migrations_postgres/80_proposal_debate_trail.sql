-- Proposal debate trail: structured objections, rebuttals, and verdicts.
--
-- The debate trail is a dedicated persistence layer for the proposal tribunal's
-- adversarial refinement workflow (epic 5bdd). Rows are typed by `kind`
-- (objection | rebuttal | verdict), carry a `blocking` flag, and track
-- resolution/reopen state — all separate from the human `proposal_feedback`
-- table whose semantics remain unchanged.
--
-- Each row is scoped to a proposal via `proposal_id` (FK + cascade delete) and
-- sequenced within a `round` so the UI can group and paginate by round. The
-- `against_revision_seq` anchors each entry to the proposal revision it was
-- written against, making staleness visible when the spec advances.

CREATE TABLE proposal_debate_trail (
    id                    VARCHAR(36)  NOT NULL PRIMARY KEY,
    proposal_id           VARCHAR(36)  NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    -- Row kind: objection | rebuttal | verdict.
    kind                  VARCHAR(16)  NOT NULL,
    -- Body text (markdown).
    body                  TEXT         NOT NULL DEFAULT '',
    -- When true, this entry blocks proposal readiness (e.g. a standing objection).
    blocking              BOOLEAN      NOT NULL DEFAULT false,
    -- Agent role that authored this entry (e.g. "advocate", "adversary", "judge").
    agent_role            VARCHAR(64)  NOT NULL DEFAULT '',
    -- Author attribution: "agent" or "user".
    author_kind           VARCHAR(16)  NOT NULL DEFAULT 'agent',
    -- FK to users table (nullable; NULL for agent-authored rows).
    author_user_id        VARCHAR(36)  NULL,
    -- Model id when author_kind == "agent".
    author_model          VARCHAR(128) NULL,
    -- Optional attribution to a specific task that produced this entry.
    source_task_id        VARCHAR(36)  NULL,
    -- The proposal revision this entry was written against.
    against_revision_seq  INT          NOT NULL DEFAULT 1,
    -- Debate round (1-based). Groups objections/rebuttals within a single
    -- refinement cycle; a new round starts when the spec is revised.
    round                 INT          NOT NULL DEFAULT 1,
    -- Resolution state (NULL while open).
    resolved_at           VARCHAR(64)  NULL,
    resolved_by_user_id   VARCHAR(36)  NULL,
    -- Reopen state (NULL while not reopened; set back to NULL on re-resolve).
    reopened_at           VARCHAR(64)  NULL,
    reopened_by_user_id   VARCHAR(36)  NULL,
    created_at            VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at            VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT proposal_debate_trail_kind_check CHECK (kind IN ('objection', 'rebuttal', 'verdict')),
    CONSTRAINT proposal_debate_trail_author_kind_check CHECK (author_kind IN ('agent', 'user'))
);

-- Ordering by proposal + round + creation for list queries.
CREATE INDEX proposal_debate_trail_proposal_round
    ON proposal_debate_trail (proposal_id, round, created_at);

-- Partial index for open blocking rows — drives readiness checks.
CREATE INDEX proposal_debate_trail_open_blocking
    ON proposal_debate_trail (proposal_id)
    WHERE blocking = true AND resolved_at IS NULL;

-- Lookup by source task (attribution).
CREATE INDEX proposal_debate_trail_source_task
    ON proposal_debate_trail (source_task_id)
    WHERE source_task_id IS NOT NULL;
