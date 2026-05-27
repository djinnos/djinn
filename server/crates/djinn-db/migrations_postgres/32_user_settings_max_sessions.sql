-- Per-user, per-model concurrency caps.
--
-- `max_sessions` (how many concurrent sessions a model may run) was a global,
-- admin-only knob that sized a shared slot pool. It now becomes per-user: each
-- user decides their own caps (e.g. 2 on gpt, 2 on kimi). Stored here as a JSON
-- object TEXT column `{ "provider/model": cap, ... }`, alongside the per-user
-- `models` selection (migration 31). NULL / absent means "no explicit caps" and
-- callers default to 1 per selected model.
--
-- There is intentionally no global ceiling and no summed total: the per-user cap
-- is the sole admission control at dispatch, and the slot pool spawns on demand
-- (Karpenter backs the pods).

ALTER TABLE user_settings ADD COLUMN max_sessions TEXT NULL;
