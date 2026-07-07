-- 7f8u — durable task arbitration storage for the park-rung arbiter.
--
-- Each arbitration row is keyed by (task_id, hold_cycle). The coordinator
-- inserts a row atomically when dispatching the Lead arbiter; uniqueness
-- guarantees at most one arbiter per hold cycle.
--
-- State machine:
--   'unconsumed'  — arbiter dispatched, decision pending
--   'consumed'    — arbiter returned a decision (approve/reject/park)
--   'failed'      — arbiter or infra failure; terminal for this cycle
--
-- Dispatch ledger fields (mirror_head_sha, github_head_sha, pr_url,
-- failing_ci_job_ids) are written once at dispatch time and read by the
-- arbiter prompt builder.
--
-- Dossier and directive are JSONB so the coordinator can pass structured
-- context without a separate join.
--
-- Monitored-reopen lifecycle fields track post-decision reopen attempts
-- so the coordinator can decide whether to re-enter the arbiter or fall
-- through to a human-review hold.

CREATE TABLE IF NOT EXISTS task_arbitrations (
    id                    VARCHAR(36)  NOT NULL PRIMARY KEY,
    task_id               VARCHAR(36)  NOT NULL,
    hold_cycle            INT          NOT NULL,
    state                 VARCHAR(64)  NOT NULL DEFAULT 'unconsumed',
    decision_failure_count INT         NOT NULL DEFAULT 0,
    infra_retry_count     INT          NOT NULL DEFAULT 0,
    deadline_at           VARCHAR(64)  NULL,
    -- Dispatch ledger
    mirror_head_sha       VARCHAR(64)  NULL,
    github_head_sha       VARCHAR(64)  NULL,
    pr_url                VARCHAR(1024) NULL,
    failing_ci_job_ids    JSONB        NOT NULL DEFAULT '[]'::jsonb,
    -- Structured payloads
    dossier               JSONB        NULL,
    directive             JSONB        NULL,
    verification_command  TEXT         NULL,
    excluded_models       JSONB        NOT NULL DEFAULT '[]'::jsonb,
    -- Monitored-reopen lifecycle
    monitored_reopen_at   VARCHAR(64)  NULL,
    monitored_reopen_count INT         NOT NULL DEFAULT 0,
    -- True once the directive has been injected into exactly one worker
    -- prompt.  Subsequent worker prompts read this flag and return None so
    -- the directive is never injected twice for the same monitored reopen.
    directive_injected    BOOLEAN      NOT NULL DEFAULT false,
    -- Timestamps
    consumed_at           VARCHAR(64)  NULL,
    created_at            VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at            VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT uq_task_arbitrations_task_cycle UNIQUE (task_id, hold_cycle),
    CONSTRAINT fk_task_arbitrations_task
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);

CREATE INDEX idx_task_arbitrations_task_id ON task_arbitrations(task_id);
CREATE INDEX idx_task_arbitrations_state   ON task_arbitrations(state);
