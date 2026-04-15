-- Staging MySQL/Dolt migration for web-client GitHub OAuth sessions.
--
-- Mirrors the SQLite refinery migration V20260415000001__user_auth_sessions.sql.
-- Table is named `user_auth_sessions` to avoid colliding with the existing
-- `sessions` table which tracks per-task agent runs.

CREATE TABLE user_auth_sessions (
    token               VARCHAR(64) NOT NULL PRIMARY KEY,
    user_id             VARCHAR(64) NOT NULL,
    github_login        VARCHAR(255) NOT NULL,
    github_name         VARCHAR(255) NULL,
    github_avatar_url   TEXT NULL,
    github_access_token TEXT NOT NULL,
    created_at          DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    expires_at          DATETIME(3) NOT NULL
);

CREATE INDEX idx_user_auth_sessions_user_id    ON user_auth_sessions(user_id);
CREATE INDEX idx_user_auth_sessions_expires_at ON user_auth_sessions(expires_at);
