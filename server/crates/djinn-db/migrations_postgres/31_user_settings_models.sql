-- Per-user model selection + priority.
--
-- The ordered model list ("which models, in what fallback order") was global,
-- living in `settings.raw` (DjinnSettings.models) and shared by every user. It
-- now becomes per-user: stored here as a JSON array of full `provider/model`
-- ids, highest priority first. NULL / absent means "no explicit selection" and
-- callers fall back to the global deployment list.
--
-- Operational caps (`max_sessions`) and `dispatch_limit` stay global/admin in
-- `settings.raw` — slots are a shared, memory-bounded resource, so per-user
-- caps can't compose into one pool.

ALTER TABLE user_settings ADD COLUMN models TEXT NULL;
