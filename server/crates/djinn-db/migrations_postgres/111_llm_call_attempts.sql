-- Durable, append-only attributed auxiliary LLM-call attempts.
--
-- One row per host-owned call attempt. The host owns timeout, stream collection,
-- payload validation, finalization, and persistence so attempted usage survives
-- post-call invalid payloads and late provider failures.
CREATE TABLE IF NOT EXISTS llm_call_attempts (
    id                          VARCHAR(36) PRIMARY KEY,
    project_id                  VARCHAR(36) NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id                     VARCHAR(36) REFERENCES tasks(id) ON DELETE CASCADE,
    task_run_id                 VARCHAR(36) REFERENCES task_runs(id) ON DELETE SET NULL,
    session_id                  VARCHAR(36) REFERENCES sessions(id) ON DELETE SET NULL,
    created_by_user_id          VARCHAR(36) REFERENCES users(id) ON DELETE SET NULL,
    operation                   VARCHAR(128) NOT NULL,
    prompt_id                   VARCHAR(128) NOT NULL,
    model_id                    VARCHAR(256) NOT NULL,
    tokens_in                   BIGINT NOT NULL DEFAULT 0 CHECK (tokens_in >= 0),
    tokens_out                  BIGINT NOT NULL DEFAULT 0 CHECK (tokens_out >= 0),
    cache_read_tokens           BIGINT NOT NULL DEFAULT 0 CHECK (cache_read_tokens >= 0),
    cache_write_tokens          BIGINT NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0),
    input_price_per_million_snapshot    DOUBLE PRECISION,
    output_price_per_million_snapshot   DOUBLE PRECISION,
    cache_read_price_per_million_snapshot   DOUBLE PRECISION,
    cache_write_price_per_million_snapshot  DOUBLE PRECISION,
    cost_usd                    DOUBLE PRECISION,
    diagnostic                  VARCHAR(512),
    outcome                     VARCHAR(32) NOT NULL CHECK (outcome IN ('success', 'timeout', 'invalid_payload', 'provider_error')),
    created_at                  VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    finalized_at                VARCHAR(64)
);

CREATE INDEX IF NOT EXISTS llm_call_attempts_project_idx
    ON llm_call_attempts (project_id);

CREATE INDEX IF NOT EXISTS llm_call_attempts_task_idx
    ON llm_call_attempts (task_id);

CREATE INDEX IF NOT EXISTS llm_call_attempts_task_run_idx
    ON llm_call_attempts (task_run_id);

CREATE INDEX IF NOT EXISTS llm_call_attempts_session_idx
    ON llm_call_attempts (session_id);

CREATE INDEX IF NOT EXISTS llm_call_attempts_operation_outcome_idx
    ON llm_call_attempts (operation, outcome);
