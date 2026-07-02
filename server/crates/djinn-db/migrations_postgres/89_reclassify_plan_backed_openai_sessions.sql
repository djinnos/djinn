-- ──────────────────────────────────────────────────────────────────────────────
-- 89: Reclassify historical PLAN-BACKED `openai/<non-codex>` sessions from
--     'actual' to 'projected' where durable credential evidence supports it.
-- ──────────────────────────────────────────────────────────────────────────────
--
-- CONTEXT
--   Migration 84 backfilled subscription/coding-plan sessions to 'projected'
--   using the stored `model_id` string, and DELIBERATELY left plain
--   `openai/<non-codex>` rows as 'actual' (see 84 comments, Family B / §(4)):
--   a bare OpenAI BYO API key is a fungible metered key, so by model_id alone an
--   `openai/gpt-5.5` row is indistinguishable from real API spend.
--
--   But when the SAME `openai/gpt-5.5` session was actually backed by a personal
--   ChatGPT/Codex PLAN OAuth credential (which surfaces its models under the
--   `openai` namespace via the `chatgpt_codex` → `openai` merge), the real API
--   spend is $0 and the correct basis is 'projected'. On prod this mis-booked
--   ~$18.5k of phantom "actual" spend. The forward runtime fix (task t41r, PART A)
--   derives the basis from the RESOLVED CREDENTIAL so NEW sessions are correct;
--   this migration repairs the pre-fix historical rows.
--
-- HEURISTIC (schema has no session→credential linkage)
--   Historical `sessions` rows carry no credential id and no `billing_source`
--   (migration 88 added the column but only forward code populates it). So we
--   cannot know per-row which credential paid. We therefore gate the whole
--   reclassification on a CONSERVATIVE, INSTALL-WIDE credential-state predicate,
--   in the same spirit as migration 84's install-wide backfill:
--
--     Reclassify `openai/<non-codex>` 'actual' rows ONLY IF, at migration time,
--     this install has a (non-revoked) ChatGPT/Codex plan OAuth credential AND
--     has NO (non-revoked) OpenAI API-key credential.
--
--   Rationale:
--     • Presence of `__OAUTH_CHATGPT_CODEX` (the ChatGPT/Codex plan OAuth token
--       DB key — see `djinn-provider::oauth::codex::CODEX_OAUTH_DB_KEY` and
--       `is_oauth_key_present`) is positive evidence the install runs openai
--       models on a plan.
--     • Absence of any `OPENAI_API_KEY` credential (the metered OpenAI key — see
--       the `openai` builtin's `required_env_vars`) means NO openai session
--       could have been metered API spend. If an OpenAI API key exists anywhere
--       in the install we do NOTHING to openai rows (they may be genuine spend).
--
--   This is intentionally all-or-nothing and conservative: it only rewrites
--   openai rows for installs that provably could not have paid per-token for
--   OpenAI. Installs that mixed a real OpenAI API key with a Codex plan are left
--   untouched (the ambiguity cannot be resolved from stored data), matching 84's
--   "leave ambiguous openai rows as actual" stance.
--
--   Codex-marked openai rows (`openai/*codex*`) are already 'projected' from
--   migration 84 Family B, so the `NOT LIKE '%codex%'` guard below only targets
--   the rows 84 deliberately skipped.
--
-- IDEMPOTENCE
--   The UPDATE is gated on `cost_basis = 'actual'`; once a row is rewritten to
--   'projected' it no longer matches, so re-running affects zero rows. The
--   install-wide credential predicate is a constant for a given DB state. No
--   guard table or idempotency key is required.
--
-- ROLLBACK
--   To reverse ONLY this migration's rewrite (restore the plan-backed openai
--   rows to 'actual'), run the same predicate with the cost_basis filter
--   inverted (SET cost_basis='actual' WHERE cost_basis='projected' AND <same
--   openai/non-codex + install-gate predicate>). Do not drop the column.
-- ──────────────────────────────────────────────────────────────────────────────

UPDATE sessions
   SET cost_basis = 'projected'
 WHERE sessions.cost_basis = 'actual'
   -- Target only the rows migration 84 deliberately left as 'actual': the
   -- `openai` namespace WITHOUT a codex marker (codex rows are already projected).
   AND lower(split_part(model_id, '/', 1)) = 'openai'
   AND lower(model_id) NOT LIKE '%codex%'
   -- Install-wide credential gate: a ChatGPT/Codex plan OAuth credential exists…
   AND EXISTS (
        SELECT 1 FROM credentials c
         WHERE c.key_name = '__OAUTH_CHATGPT_CODEX'
           AND c.revoked_at IS NULL
   )
   -- …and NO OpenAI API-key credential exists anywhere in the install.
   AND NOT EXISTS (
        SELECT 1 FROM credentials c
         WHERE c.key_name = 'OPENAI_API_KEY'
           AND c.revoked_at IS NULL
   );

-- ══════════════════════════════════════════════════════════════════════════════
-- VALIDATION / OBSERVABILITY QUERIES  (NOT auto-executed — commented out)
-- ══════════════════════════════════════════════════════════════════════════════
-- (1) Install-gate state — does this install qualify for the rewrite at all?
-- SELECT
--   EXISTS (SELECT 1 FROM credentials WHERE key_name = '__OAUTH_CHATGPT_CODEX'
--             AND revoked_at IS NULL)                      AS has_codex_plan_oauth,
--   EXISTS (SELECT 1 FROM credentials WHERE key_name = 'OPENAI_API_KEY'
--             AND revoked_at IS NULL)                      AS has_openai_api_key;
-- Expectation for a qualifying install: (has_codex_plan_oauth, has_openai_api_key)
-- = (true, false). Any other combination reclassifies ZERO rows.
--
-- (2) Candidate rows BEFORE the rewrite (predict volume / dollar swing):
-- SELECT COUNT(*) AS candidate_rows, COALESCE(SUM(cost_usd), 0) AS candidate_usd
--   FROM sessions
--  WHERE cost_basis = 'actual'
--    AND lower(split_part(model_id, '/', 1)) = 'openai'
--    AND lower(model_id) NOT LIKE '%codex%';
-- After a qualifying run these move from 'actual' to 'projected'; their USD
-- leaves the "actual spend" aggregate (correctly — real spend was $0).
--
-- (3) Residual openai 'actual' AFTER: for a qualifying install this must be 0
--     (all non-codex openai rows became projected). For a non-qualifying install
--     it is unchanged.
-- SELECT COUNT(*) FROM sessions
--  WHERE cost_basis = 'actual'
--    AND lower(split_part(model_id, '/', 1)) = 'openai'
--    AND lower(model_id) NOT LIKE '%codex%';
