-- Migration 138: durable refinement-run ownership and exact correlation.
--
-- This is deliberately additive. Existing coordinator paths continue to use
-- proposal_revisions.event_metadata while mixed-version deployments introduce
-- run/intent identity. Historical correlation is restricted to rows whose
-- proposal and start/stop interval provide unambiguous evidence.

CREATE TABLE refinement_runs (
    id                   VARCHAR(36) NOT NULL PRIMARY KEY,
    proposal_id          VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    generation           INT NOT NULL,
    idempotency_key      VARCHAR(255) NOT NULL,
    state                VARCHAR(16) NOT NULL DEFAULT 'running',
    source_start_revision_id VARCHAR(36) NULL UNIQUE REFERENCES proposal_revisions(id) ON DELETE SET NULL,
    started_at           VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    heartbeat_at         VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    parked_at            VARCHAR(64) NULL,
    park_kind            VARCHAR(64) NULL,
    terminal_at          VARCHAR(64) NULL,
    stop_tag             VARCHAR(32) NULL,
    stop_context         JSONB NULL,
    created_at           VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at           VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT refinement_runs_generation_unique UNIQUE (proposal_id, generation),
    CONSTRAINT refinement_runs_idempotency_unique UNIQUE (proposal_id, idempotency_key),
    CONSTRAINT refinement_runs_state_check CHECK (state IN ('running', 'parked', 'terminal')),
    CONSTRAINT refinement_runs_park_check CHECK (
        (state = 'parked' AND parked_at IS NOT NULL AND park_kind IS NOT NULL)
        OR (state <> 'parked' AND parked_at IS NULL AND park_kind IS NULL)
    ),
    CONSTRAINT refinement_runs_terminal_check CHECK (
        (state = 'terminal' AND terminal_at IS NOT NULL AND stop_tag IS NOT NULL)
        OR (state <> 'terminal' AND terminal_at IS NULL AND stop_tag IS NULL AND stop_context IS NULL)
    ),
    CONSTRAINT refinement_runs_stop_tag_check CHECK (stop_tag IS NULL OR stop_tag IN (
        'adversary_dry', 'round_cap', 'spawn_cap', 'repeated_objection',
        'agent_failure', 'human_accepted', 'human_rejected', 'interrupted',
        'reaped_phantom', 'operator_stop', 'unknown_legacy'
    ))
);

CREATE UNIQUE INDEX refinement_runs_one_nonterminal_per_proposal
    ON refinement_runs(proposal_id) WHERE state IN ('running', 'parked');
CREATE INDEX idx_refinement_runs_proposal_state
    ON refinement_runs(proposal_id, state, generation DESC);

CREATE TABLE refinement_dispatch_intents (
    id                   VARCHAR(36) NOT NULL PRIMARY KEY,
    run_id               VARCHAR(36) NOT NULL REFERENCES refinement_runs(id) ON DELETE CASCADE,
    round                INT NOT NULL,
    phase                VARCHAR(64) NOT NULL,
    role                 VARCHAR(64) NOT NULL,
    state                VARCHAR(16) NOT NULL DEFAULT 'pending',
    idempotency_key      VARCHAR(255) NOT NULL,
    claimed_by           VARCHAR(255) NULL,
    claimed_at           VARCHAR(64) NULL,
    claim_expires_at     VARCHAR(64) NULL,
    task_id              VARCHAR(36) NULL REFERENCES tasks(id) ON DELETE SET NULL,
    created_at           VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at           VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    terminal_at          VARCHAR(64) NULL,
    CONSTRAINT refinement_dispatch_intents_round_check CHECK (round > 0),
    CONSTRAINT refinement_dispatch_intents_state_check CHECK (state IN ('pending', 'claimed', 'materialized', 'completed', 'cancelled')),
    CONSTRAINT refinement_dispatch_intents_claim_check CHECK (
        (state = 'claimed' AND claimed_by IS NOT NULL AND claimed_at IS NOT NULL AND claim_expires_at IS NOT NULL)
        OR (state <> 'claimed' AND claimed_by IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL)
    ),
    CONSTRAINT refinement_dispatch_intents_terminal_check CHECK (
        (state IN ('completed', 'cancelled') AND terminal_at IS NOT NULL)
        OR (state NOT IN ('completed', 'cancelled') AND terminal_at IS NULL)
    ),
    CONSTRAINT refinement_dispatch_intents_identity_unique UNIQUE (run_id, round, phase, role),
    CONSTRAINT refinement_dispatch_intents_idempotency_unique UNIQUE (run_id, idempotency_key)
);
CREATE INDEX idx_refinement_dispatch_intents_run_state
    ON refinement_dispatch_intents(run_id, state, round);
CREATE INDEX idx_refinement_dispatch_intents_claim_expiry
    ON refinement_dispatch_intents(claim_expires_at) WHERE state = 'claimed';

ALTER TABLE tasks
    ADD COLUMN refinement_run_id VARCHAR(36) NULL REFERENCES refinement_runs(id) ON DELETE SET NULL,
    ADD COLUMN refinement_intent_id VARCHAR(36) NULL REFERENCES refinement_dispatch_intents(id) ON DELETE SET NULL;
CREATE INDEX idx_tasks_refinement_run_id ON tasks(refinement_run_id) WHERE refinement_run_id IS NOT NULL;
CREATE INDEX idx_tasks_refinement_intent_id ON tasks(refinement_intent_id) WHERE refinement_intent_id IS NOT NULL;

ALTER TABLE proposal_debate_trail
    ADD COLUMN refinement_run_id VARCHAR(36) NULL REFERENCES refinement_runs(id) ON DELETE SET NULL;
CREATE INDEX idx_proposal_debate_trail_refinement_run
    ON proposal_debate_trail(refinement_run_id) WHERE refinement_run_id IS NOT NULL;

ALTER TABLE proposal_revisions
    ADD COLUMN refinement_run_id VARCHAR(36) NULL REFERENCES refinement_runs(id) ON DELETE SET NULL,
    ADD COLUMN refinement_stop_tag VARCHAR(32) NULL,
    ADD COLUMN refinement_stop_context JSONB NULL,
    ADD CONSTRAINT proposal_revisions_refinement_stop_tag_check CHECK (refinement_stop_tag IS NULL OR refinement_stop_tag IN (
        'adversary_dry', 'round_cap', 'spawn_cap', 'repeated_objection',
        'agent_failure', 'human_accepted', 'human_rejected', 'interrupted',
        'reaped_phantom', 'operator_stop', 'unknown_legacy'
    ));
CREATE INDEX idx_proposal_revisions_refinement_run
    ON proposal_revisions(refinement_run_id) WHERE refinement_run_id IS NOT NULL;

-- A stable UUID-compatible legacy id is derived solely from the immutable
-- lifecycle start row. Reapplying an identical source dataset yields the same
-- id and generation. Each start owns the following stop, if any.
WITH starts AS (
    SELECT r.id, r.proposal_id, r.created_at,
           row_number() OVER (PARTITION BY r.proposal_id ORDER BY r.created_at, r.id)::INT AS generation,
           (SELECT s.id FROM proposal_revisions s
             WHERE s.proposal_id = r.proposal_id
               AND s.event_kind = 'refinement_stop'
               AND (s.created_at, s.id) > (r.created_at, r.id)
             ORDER BY s.created_at, s.id LIMIT 1) AS stop_id
    FROM proposal_revisions r WHERE r.event_kind = 'refinement_start'
), boundaries AS (
    SELECT starts.*, stop.created_at AS stop_at,
           stop.event_metadata AS stop_metadata
    FROM starts LEFT JOIN proposal_revisions stop ON stop.id = starts.stop_id
), normalized AS (
    SELECT *, CASE
      WHEN stop_id IS NULL THEN NULL
      WHEN COALESCE(stop_metadata->>'reason_tag', stop_metadata->>'stop_reason', stop_metadata->>'reason') IN
        ('adversary_dry','round_cap','spawn_cap','repeated_objection','agent_failure','human_accepted','human_rejected','interrupted','reaped_phantom','operator_stop')
        THEN COALESCE(stop_metadata->>'reason_tag', stop_metadata->>'stop_reason', stop_metadata->>'reason')
      WHEN COALESCE(stop_metadata->>'reason_tag', stop_metadata->>'stop_reason', stop_metadata->>'reason') IN ('judge_converged','dry_rounds') THEN 'adversary_dry'
      ELSE 'unknown_legacy' END AS stop_tag
    FROM boundaries
)
INSERT INTO refinement_runs (id, proposal_id, generation, idempotency_key, state, source_start_revision_id, started_at, heartbeat_at, terminal_at, stop_tag, stop_context)
SELECT substr(md5('refinement-run:' || id),1,8) || '-' || substr(md5('refinement-run:' || id),9,4) || '-' || substr(md5('refinement-run:' || id),13,4) || '-' || substr(md5('refinement-run:' || id),17,4) || '-' || substr(md5('refinement-run:' || id),21,12),
       proposal_id, generation, 'legacy-start:' || id,
       CASE WHEN stop_id IS NULL THEN 'running' ELSE 'terminal' END,
       id, created_at, created_at, stop_at, stop_tag,
       CASE WHEN stop_id IS NULL THEN NULL ELSE jsonb_build_object('legacy_source_revision_id', stop_id, 'legacy_metadata', COALESCE(stop_metadata, '{}'::jsonb)) END
FROM normalized;

-- Lifecycle rows receive their exact interval run. Only start, its following
-- stop, and rows strictly inside that interval are correlated.
UPDATE proposal_revisions r SET refinement_run_id = runs.id
FROM refinement_runs runs
WHERE runs.source_start_revision_id = r.id;
UPDATE proposal_revisions stop SET refinement_run_id = runs.id,
    refinement_stop_tag = runs.stop_tag, refinement_stop_context = runs.stop_context
FROM (
    -- A shared following stop is ambiguous for nested legacy starts. As with
    -- interior rows, correlate it only when exactly one run names that row.
    SELECT stop_row.id, min(run_row.id) AS run_id
    FROM proposal_revisions stop_row
    JOIN refinement_runs run_row
      ON stop_row.id = (run_row.stop_context->>'legacy_source_revision_id')
    WHERE run_row.terminal_at IS NOT NULL
      AND stop_row.event_kind = 'refinement_stop'
      AND stop_row.proposal_id = run_row.proposal_id
      AND stop_row.created_at = run_row.terminal_at
    GROUP BY stop_row.id
    HAVING count(*) = 1
) candidate
JOIN refinement_runs runs ON runs.id = candidate.run_id
WHERE stop.id = candidate.id;
UPDATE proposal_revisions r SET refinement_run_id = candidate.run_id
FROM (
    -- A nested/overlapping legacy interval is not evidence for either run.
    -- Do not let UPDATE ... FROM select an arbitrary matching run.
    SELECT revision.id, min(runs.id) AS run_id
    FROM proposal_revisions revision
    JOIN refinement_runs runs ON runs.proposal_id = revision.proposal_id
    JOIN proposal_revisions start ON start.id = runs.source_start_revision_id
    WHERE revision.event_kind NOT IN ('refinement_start', 'refinement_stop')
      AND (revision.created_at, revision.id) > (start.created_at, start.id)
      AND (runs.terminal_at IS NULL OR revision.created_at < runs.terminal_at)
    GROUP BY revision.id
    HAVING count(*) = 1
) candidate
WHERE r.id = candidate.id;

-- Debate/task attribution is intentionally evidence-only: an unambiguous
-- debate source task inside a run interval may correlate its task; no proposal
-- or time-only task guesses are made.
UPDATE proposal_debate_trail d SET refinement_run_id = candidate.run_id
FROM (
    SELECT d2.id, min(runs.id) AS run_id
    FROM proposal_debate_trail d2 JOIN refinement_runs runs ON runs.proposal_id = d2.proposal_id
    JOIN proposal_revisions start ON start.id = runs.source_start_revision_id
    WHERE (d2.created_at, d2.id) > (start.created_at, start.id)
      AND (runs.terminal_at IS NULL OR d2.created_at < runs.terminal_at)
    GROUP BY d2.id HAVING count(*) = 1
) candidate WHERE d.id = candidate.id;
UPDATE tasks t SET refinement_run_id = candidate.run_id
FROM (
    SELECT d.source_task_id, min(d.refinement_run_id) AS run_id
    FROM proposal_debate_trail d WHERE d.source_task_id IS NOT NULL AND d.refinement_run_id IS NOT NULL
    GROUP BY d.source_task_id HAVING count(DISTINCT d.refinement_run_id) = 1
) candidate WHERE t.id = candidate.source_task_id;
