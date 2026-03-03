-- Credential vault: encrypted API key storage for Goose provider dispatch.
--
-- key_name is UNIQUE — one stored value per env-var name (e.g. ANTHROPIC_API_KEY).
-- encrypted_value stores: nonce (12 bytes) || AES-256-GCM ciphertext+tag.
-- The encryption key is derived from machine identity (hostname + user).

CREATE TABLE credentials (
    id              TEXT NOT NULL PRIMARY KEY,
    provider_id     TEXT NOT NULL,
    key_name        TEXT NOT NULL UNIQUE,
    encrypted_value BLOB NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX credentials_provider_id ON credentials(provider_id);
