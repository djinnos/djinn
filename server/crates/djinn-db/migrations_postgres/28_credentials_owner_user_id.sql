-- Per-user provider credentials with org-shared fallback.
--
-- Today `credentials` is GLOBAL: a single row per `key_name`, selected by
-- `WHERE key_name = $1`. That means whichever user most recently connected a
-- provider (e.g. their personal Codex OAuth) has their key used for EVERYONE's
-- task runs and chat turns. This migration makes credentials per-user with an
-- org-shared fallback:
--
--   * `owner_user_id` references `users(id)`.
--   * `owner_user_id IS NULL`     → org-shared / fallback credential (the
--     historical behaviour; any user with no personal credential resolves to
--     this one).
--   * `owner_user_id = <user id>` → that user's private credential, which the
--     resolver prefers over the org-shared one when the acting user matches.
--
-- The single global `UNIQUE(key_name)` constraint is replaced with two PARTIAL
-- unique indexes so that:
--   * at most one org-shared row exists per `key_name`, and
--   * at most one private row exists per `(key_name, owner_user_id)`,
-- while still allowing one org-shared row to coexist with N per-user rows that
-- share the same `key_name`.
--
-- `custom_providers` (OpenAI-compatible endpoint registrations) stays
-- org-shared on purpose: those rows hold base URLs / env-var names, not
-- secrets, so there's no isolation requirement.

ALTER TABLE credentials
    ADD COLUMN owner_user_id VARCHAR(36) NULL,
    ADD CONSTRAINT fk_credentials_owner_user
        FOREIGN KEY (owner_user_id) REFERENCES users(id) ON DELETE CASCADE;

-- Drop the global single-row-per-key_name constraint so a key_name can have
-- both an org-shared row and per-user rows.
ALTER TABLE credentials
    DROP CONSTRAINT uq_credentials_key_name;

-- Exactly one org-shared (fallback) row per key_name.
CREATE UNIQUE INDEX uq_credentials_key_name_org
    ON credentials (key_name)
    WHERE owner_user_id IS NULL;

-- Exactly one private row per (key_name, owner_user_id).
CREATE UNIQUE INDEX uq_credentials_key_name_owner
    ON credentials (key_name, owner_user_id)
    WHERE owner_user_id IS NOT NULL;

CREATE INDEX idx_credentials_owner_user_id ON credentials (owner_user_id);
