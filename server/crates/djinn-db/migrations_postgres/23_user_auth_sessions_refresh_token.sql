-- Make `user_auth_sessions` aware of the GitHub-side token lifecycle.
--
-- Three new columns; all nullable so non-expiring App configs still work:
--   * github_access_token_expires_at — when GitHub's access_token dies.
--   * github_refresh_token + github_refresh_token_expires_at — the
--     paired refresh credential and its own (much longer) deadline.

ALTER TABLE user_auth_sessions
    ADD COLUMN github_access_token_expires_at  VARCHAR(64) NULL,
    ADD COLUMN github_refresh_token            TEXT        NULL,
    ADD COLUMN github_refresh_token_expires_at VARCHAR(64) NULL;
