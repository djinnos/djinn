-- Phase 1 of "one deployment = one GitHub org":
--   1. `org_config`  — singleton row (id=1) describing the GitHub org this
--                      deployment is locked to.
--   2. `users`       — persistent identity table keyed on `github_id`.
--   3. `user_fk` / `created_by_user_id` / `assignee_user_id` attribution
--      columns added (nullable) to `user_auth_sessions`, `tasks`, `epics`,
--      and `sessions`. Phase 2 will backfill + drop the denormalised
--      `github_*` columns on `user_auth_sessions`.
--
-- Forward-only: no rebuild of `1_initial_schema.sql`. Existing rows get
-- NULL in the new columns.

-- ── org_config (singleton via CHECK (id = 1)) ────────────────────────────────
CREATE TABLE IF NOT EXISTS org_config (
    id                INT          NOT NULL PRIMARY KEY,
    github_org_id     BIGINT       NOT NULL,
    github_org_login  VARCHAR(255) NOT NULL,
    app_id            BIGINT       NOT NULL,
    installation_id   BIGINT       NOT NULL,
    created_at        VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    CONSTRAINT chk_org_config_singleton CHECK (id = 1)
);

-- ── users (stable identity backed by GitHub) ─────────────────────────────────
CREATE TABLE IF NOT EXISTS users (
    id                 VARCHAR(36)  NOT NULL PRIMARY KEY,
    github_id          BIGINT       NOT NULL,
    github_login       VARCHAR(255) NOT NULL,
    github_name        VARCHAR(255) NULL,
    github_avatar_url  TEXT         NULL,
    is_member_of_org   BOOLEAN      NOT NULL DEFAULT TRUE,
    last_seen_at       VARCHAR(64)  NULL,
    created_at         VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    UNIQUE KEY uq_users_github_id (github_id)
);

CREATE INDEX idx_users_github_login ON users(github_login);

-- ── Attribution FKs (nullable; Phase 2 will backfill) ────────────────────────
ALTER TABLE user_auth_sessions
    ADD COLUMN user_fk VARCHAR(36) NULL,
    ADD CONSTRAINT fk_user_auth_sessions_user
        FOREIGN KEY (user_fk) REFERENCES users(id) ON DELETE SET NULL;

CREATE INDEX idx_user_auth_sessions_user_fk ON user_auth_sessions(user_fk);

ALTER TABLE tasks
    ADD COLUMN created_by_user_id VARCHAR(36) NULL,
    ADD COLUMN assignee_user_id   VARCHAR(36) NULL,
    ADD CONSTRAINT fk_tasks_created_by_user
        FOREIGN KEY (created_by_user_id) REFERENCES users(id) ON DELETE SET NULL,
    ADD CONSTRAINT fk_tasks_assignee_user
        FOREIGN KEY (assignee_user_id)   REFERENCES users(id) ON DELETE SET NULL;

CREATE INDEX idx_tasks_created_by_user_id ON tasks(created_by_user_id);
CREATE INDEX idx_tasks_assignee_user_id   ON tasks(assignee_user_id);

ALTER TABLE epics
    ADD COLUMN created_by_user_id VARCHAR(36) NULL,
    ADD CONSTRAINT fk_epics_created_by_user
        FOREIGN KEY (created_by_user_id) REFERENCES users(id) ON DELETE SET NULL;

CREATE INDEX idx_epics_created_by_user_id ON epics(created_by_user_id);

ALTER TABLE sessions
    ADD COLUMN created_by_user_id VARCHAR(36) NULL,
    ADD CONSTRAINT fk_sessions_created_by_user
        FOREIGN KEY (created_by_user_id) REFERENCES users(id) ON DELETE SET NULL;

CREATE INDEX idx_sessions_created_by_user_id ON sessions(created_by_user_id);
