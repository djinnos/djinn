-- Org-wide AI policy (admin-owned, singleton).
--
-- Slice 5 of proposal p8py ("Model setup UX"). Adds an admin-only org policy
-- surface governing:
--
--   * Subscription allow/block, framed by data residency. `blocked_subscriptions`
--     is a JSON array of subscription provider ids that are filtered OUT of every
--     member-facing provider/model list and rejected by per-user model validation.
--     Admin API keys (non-subscription providers) are NEVER governed here.
--   * Org-default per-role lane assignment (plan/implement/review) that new
--     members inherit when they have none, plus a lock level: `flexible` (members
--     may override) vs `locked` (org assignment is authoritative).
--
-- This is a singleton: exactly one row, pinned by a CHECK on a constant primary
-- key, so reads/writes never need a key argument. JSON-object/array columns
-- mirror the per-user `user_settings` storage style (TEXT holding JSON) rather
-- than introducing typed array/jsonb columns, keeping the read path identical.

CREATE TABLE IF NOT EXISTS org_ai_policy (
    -- Singleton guard: only the row with id = 1 may ever exist.
    id                     INTEGER     NOT NULL PRIMARY KEY DEFAULT 1,
    -- JSON array of blocked subscription provider ids, e.g. ["minimax-coding-plan"].
    blocked_subscriptions  TEXT        NOT NULL DEFAULT '[]',
    -- JSON object { "plan": [...], "implement": [...], "review": [...] } of the
    -- org-default per-role lanes new members inherit. '{}'/all-empty = no default.
    default_lanes          TEXT        NOT NULL DEFAULT '{}',
    -- 'flexible' (members may override) | 'locked' (org assignment authoritative).
    lock_level             VARCHAR(16) NOT NULL DEFAULT 'flexible',
    -- JSON array of fully-qualified "provider/model-id" entries to ADD to the
    -- recommended set on top of the baseline RECOMMENDED_MODELS list.
    additional_recommended_model_ids  TEXT NOT NULL DEFAULT '[]',
    -- JSON array of fully-qualified "provider/model-id" entries to REMOVE from
    -- the recommended set (demotion wins over addition).
    demoted_recommended_model_ids     TEXT NOT NULL DEFAULT '[]',
    updated_at             VARCHAR(64) NOT NULL DEFAULT '',
    CONSTRAINT org_ai_policy_singleton CHECK (id = 1),
    CONSTRAINT org_ai_policy_lock_level_check CHECK (lock_level IN ('flexible', 'locked'))
);

-- Seed the singleton row so reads never have to branch on row existence.
INSERT INTO org_ai_policy (id) VALUES (1)
ON CONFLICT (id) DO NOTHING;
