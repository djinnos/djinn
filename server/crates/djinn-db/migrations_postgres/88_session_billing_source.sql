-- Add per-session `billing_source` column recording the KIND of credential that
-- backed the session, so plan-vs-API-key usage is queryable after the fact.
--
-- Motivation:
--   `cost_basis` (migration 83) records how to interpret `cost_usd` (actual /
--   projected / unpriced). It does NOT record what kind of credential produced
--   the session. In particular a session on `openai/<non-codex>` backed by a
--   personal ChatGPT/Codex plan OAuth credential incurs $0 real API spend, yet
--   is indistinguishable from a metered OpenAI API-key session by model_id
--   alone. `billing_source` captures that credential kind explicitly.
--
-- Values:
--   'plan_oauth' — the session was backed by a personal subscription-plan OAuth
--                  credential (e.g. ChatGPT/Codex plan, GitHub Copilot). Real
--                  per-token API spend is $0; `cost_usd` is a projection.
--   'api_key'    — the session was backed by an API key (metered pay-as-you-go
--                  OR a coding-plan API key such as `zai-coding-plan`). For
--                  coding-plan API keys the plan nature is already captured by
--                  `cost_basis = 'projected'`; `billing_source` exists chiefly to
--                  flag the OAuth-plan case the model_id cannot reveal.
--   NULL         — unknown / not recorded (legacy rows, and non-billing session
--                  kinds such as interactive `chat` and post-session extraction
--                  helpers that carry no dispatch-time credential signal).
--
-- Nullable by design: unlike `cost_basis` (NOT NULL DEFAULT 'unpriced'), the
-- credential kind is only known for dispatch-created task sessions. NULL is the
-- honest "not recorded" value for everything else, and the forward runtime code
-- (djinn-agent `create_session`) stamps a concrete value for task sessions.
--
-- No credential foreign key is introduced; this is a denormalized label written
-- at session creation from the resolved credential kind, mirroring the design of
-- `cost_basis`.

ALTER TABLE sessions
    ADD COLUMN billing_source TEXT NULL
        CONSTRAINT sessions_billing_source_check
            CHECK (billing_source IS NULL OR billing_source IN ('plan_oauth', 'api_key'));
