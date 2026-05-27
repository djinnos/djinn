-- Admin role.
--
-- djinn had no notion of an admin: every authenticated user could mutate the
-- global runtime settings (dispatch limit, per-model concurrency caps, the
-- deployment fallback model list). With per-user model selection landing, those
-- global/operational knobs need an owner. We add a single boolean privilege bit.
--
-- Bootstrap: the FIRST user to sign in (the login that authenticates when no
-- admin yet exists) is stamped admin by the auth callback. Further changes are
-- manual `UPDATE users SET is_admin = TRUE WHERE github_login = '…'` for now —
-- there is intentionally no env allowlist or in-app toggle yet.

ALTER TABLE users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT FALSE;
