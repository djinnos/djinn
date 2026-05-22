-- Make `user_auth_sessions` aware of the GitHub-side token lifecycle.
--
-- The previous schema conflated two unrelated TTLs into one `expires_at`
-- column:
--   * The browser session — random 32-byte token in the `djinn_session`
--     cookie, intentionally long-lived (30d) so users stay signed in.
--   * The GitHub access token — issued by `/login/oauth/access_token`,
--     typically lives ~8h when "Expire user authorization tokens" is on,
--     and is refreshable via the paired refresh token.
--
-- Treating them as a single deadline meant the row stayed "valid" for
-- 30 days after the underlying GitHub token had already died, and the
-- pr_poller's auto-approve path looped on 401s without ever knowing it
-- could refresh.
--
-- Three new columns; all nullable so non-expiring App configs still work:
--   * github_access_token_expires_at — when GitHub's access_token dies.
--   * github_refresh_token + github_refresh_token_expires_at — the
--     paired refresh credential and its own (much longer) deadline.
--
-- Existing rows backfill to NULL on every new column; they'll be picked
-- up by the transport's 401 path, which falls back to "treat as expired"
-- when no refresh token is on file.

ALTER TABLE user_auth_sessions
    ADD COLUMN github_access_token_expires_at  VARCHAR(64) NULL AFTER github_access_token,
    ADD COLUMN github_refresh_token            TEXT        NULL AFTER github_access_token_expires_at,
    ADD COLUMN github_refresh_token_expires_at VARCHAR(64) NULL AFTER github_refresh_token;
