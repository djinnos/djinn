CREATE TABLE IF NOT EXISTS dispatch_state (
    task_id VARCHAR(36) PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    failure_streak INTEGER NOT NULL DEFAULT 0,
    cooldown_until TIMESTAMPTZ,
    escalation_count INTEGER NOT NULL DEFAULT 0,
    last_dispatched_at TIMESTAMPTZ,
    last_dispatched_role TEXT,
    inflight_creator_user_id TEXT,
    inflight_model_id TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_dispatch_state_cooldown_until
    ON dispatch_state (cooldown_until)
    WHERE cooldown_until IS NOT NULL;

