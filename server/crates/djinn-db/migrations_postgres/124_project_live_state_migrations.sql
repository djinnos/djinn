-- Release N live-state migration reconciliation authority. Additive: old
-- project-local inputs remain under their family-specific rollback policy.
CREATE TABLE IF NOT EXISTS project_live_state_migrations (
    project_id           VARCHAR(36)  NOT NULL,
    family               VARCHAR(255) NOT NULL,
    release              VARCHAR(64)  NOT NULL,
    source_inventory     JSONB        NOT NULL,
    destination          VARCHAR(1024) NOT NULL,
    pre_hash             VARCHAR(128) NULL,
    post_hash            VARCHAR(128) NULL,
    started_at           VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at           VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    finalized_at         VARCHAR(64)  NULL,
    result               VARCHAR(32)  NOT NULL DEFAULT 'pending',
    detail               TEXT         NULL,
    rollback_instruction TEXT         NOT NULL,
    PRIMARY KEY (project_id, family, release),
    CONSTRAINT fk_project_live_state_migrations_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT project_live_state_migrations_result_check
        CHECK (result IN ('pending', 'succeeded', 'failed', 'rolled_back'))
);

CREATE INDEX IF NOT EXISTS project_live_state_migrations_project_result
    ON project_live_state_migrations (project_id, result, family);
