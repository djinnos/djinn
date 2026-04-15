-- Web client GitHub OAuth session rows.
--
-- Separate from `sessions` (which tracks agent/task runs). Each row represents
-- a logged-in browser user session backed by a random 32-byte token stored in
-- the `djinn_session` cookie.

CREATE TABLE user_auth_sessions (
    token              TEXT NOT NULL PRIMARY KEY,
    user_id            TEXT NOT NULL,
    github_login       TEXT NOT NULL,
    github_name        TEXT,
    github_avatar_url  TEXT,
    github_access_token TEXT NOT NULL,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at         TEXT NOT NULL
);

CREATE INDEX idx_user_auth_sessions_user_id ON user_auth_sessions(user_id);
CREATE INDEX idx_user_auth_sessions_expires_at ON user_auth_sessions(expires_at);
