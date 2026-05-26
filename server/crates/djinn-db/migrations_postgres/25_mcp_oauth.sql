-- MCP OAuth 2.1 support: djinn becomes both an OAuth Resource Server (RS) and
-- its own Authorization Server (AS), federating the existing GitHub login as
-- the upstream identity provider.
--
-- Three tables:
--   1. oauth_clients              — RFC 7591 Dynamic Client Registration.
--   2. oauth_authorization_codes  — short-lived (~60s) single-use auth codes.
--   3. mcp_access_tokens          — issued opaque bearer + refresh tokens.
--
-- All tokens are opaque DB-backed values (NOT JWTs); validation is a DB lookup
-- (expiry + audience), mirroring the `user_auth_sessions` cookie pattern.
-- Timestamps follow the existing convention: RFC3339 strings in VARCHAR(64)
-- columns, lexicographically comparable, written via to_char(now() …).

-- ── oauth_clients (dynamic client registration) ──────────────────────────────
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id                  VARCHAR(64)  NOT NULL PRIMARY KEY,
    -- NULL for public PKCE clients (token_endpoint_auth_method = 'none').
    client_secret              TEXT         NULL,
    -- JSONB array of registered redirect URIs (exact-match validated).
    redirect_uris              JSONB        NOT NULL,
    client_name                VARCHAR(255) NULL,
    token_endpoint_auth_method VARCHAR(64)  NOT NULL DEFAULT 'none',
    -- JSONB array of grant types, e.g. ["authorization_code","refresh_token"].
    grant_types                JSONB        NOT NULL,
    created_at                 VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);

-- ── oauth_authorization_codes (single-use, ~60s TTL) ─────────────────────────
CREATE TABLE IF NOT EXISTS oauth_authorization_codes (
    code                  VARCHAR(64)  NOT NULL PRIMARY KEY,
    client_id             VARCHAR(64)  NOT NULL,
    redirect_uri          TEXT         NOT NULL,
    -- PKCE: only S256 is accepted (enforced in application code).
    code_challenge        TEXT         NOT NULL,
    code_challenge_method VARCHAR(16)  NOT NULL,
    -- The resource (audience) the eventual token is bound to: {base}/mcp.
    resource              TEXT         NOT NULL,
    scope                 VARCHAR(255) NULL,
    -- FK into users.id — the human who authorized the code.
    user_id               VARCHAR(36)  NOT NULL,
    expires_at            VARCHAR(64)  NOT NULL,
    -- Set the moment the code is exchanged; a non-NULL value means "spent".
    consumed_at           VARCHAR(64)  NULL,
    created_at            VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_oauth_codes_client
        FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    CONSTRAINT fk_oauth_codes_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX idx_oauth_codes_client_id ON oauth_authorization_codes(client_id);
CREATE INDEX idx_oauth_codes_user_id   ON oauth_authorization_codes(user_id);

-- ── mcp_access_tokens (issued bearer tokens) ─────────────────────────────────
CREATE TABLE IF NOT EXISTS mcp_access_tokens (
    token                     VARCHAR(64)  NOT NULL PRIMARY KEY,
    -- Random opaque refresh credential. NULL when refresh is not issued.
    refresh_token             VARCHAR(64)  NULL,
    client_id                 VARCHAR(64)  NOT NULL,
    -- FK into users.id — attribution target for MCP-created tasks.
    user_id                   VARCHAR(36)  NOT NULL,
    -- Audience: the RS validates incoming bearer tokens have resource={base}/mcp.
    resource                  TEXT         NOT NULL,
    scope                     VARCHAR(255) NULL,
    expires_at                VARCHAR(64)  NOT NULL,
    refresh_token_expires_at  VARCHAR(64)  NULL,
    created_at                VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    -- Set on revocation (including refresh-token rotation of the old token).
    revoked_at                VARCHAR(64)  NULL,
    CONSTRAINT fk_mcp_tokens_client
        FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    CONSTRAINT fk_mcp_tokens_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX idx_mcp_tokens_refresh_token ON mcp_access_tokens(refresh_token);
CREATE INDEX idx_mcp_tokens_user_id       ON mcp_access_tokens(user_id);
CREATE INDEX idx_mcp_tokens_client_id     ON mcp_access_tokens(client_id);
