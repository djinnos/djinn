-- ──────────────────────────────────────────────────────────────────────────────
-- 141: Backfill cost_basis for historical sessions mis-booked before the
--      billing-hint forward fix (derive_billing_signal, #2352) shipped.
-- ──────────────────────────────────────────────────────────────────────────────
--
-- CONTEXT
--   Two classes of historical `sessions` rows carry the wrong `cost_basis`:
--
--     (1) ZERO-PRICED rows booked as real spend. Migration 83 backfilled every
--         row with a non-NULL price snapshot to 'actual', and later runtime code
--         booked flat-rate coding-plan sessions (whose catalog pricing is
--         all-zero) as 'projected' $0.00. A zero-priced session is neither real
--         API spend nor a meaningful projection — it is 'unpriced'. The forward
--         fix (task, PART B) now requires NON-ZERO pricing for both the metered
--         ('actual') and subscription ('projected') arms of `determine_cost_basis`.
--         This migration repairs the pre-fix rows.
--
--     (2) PLAN-BACKED `openai/*` rows booked as 'actual'. Before #2352, an
--         openai-namespace session backed by a personal ChatGPT/Codex PLAN OAuth
--         credential (real API spend = $0) was priced and booked 'actual' by the
--         legacy `determine_cost_basis` path. `derive_billing_signal` now emits a
--         SubscriptionPlan hint for those sessions, but no backfill of the
--         historical rows was ever shipped for the full `openai/*` set.
--
-- ORDER MATTERS
--   Step 1 (zero-priced → 'unpriced') MUST run before step 2 (openai 'actual' →
--   'projected'). After step 1, any zero-priced openai 'actual' row has already
--   become 'unpriced', so step 2 only promotes genuinely priced openai rows to
--   'projected' and never resurrects a zero-priced $0.00 as projected spend.
--
-- IDEMPOTENCE
--   Both statements are gated on the cost_basis they rewrite AWAY from, so a
--   re-run matches zero rows. No guard table or idempotency key is required.
-- ──────────────────────────────────────────────────────────────────────────────


-- ── STEP 1: zero-priced sessions → 'unpriced' ────────────────────────────────
--
-- Reclassify any 'actual' / 'projected' row whose FOUR pricing-snapshot columns
-- are all literally zero. NULL snapshots must NOT match (a NULL snapshot means
-- "no pricing recorded", already handled by 83's 'unpriced' default and not this
-- migration's concern). Only an all-literal-zero snapshot — the flat-rate
-- subscription / coding-plan signature — is rewritten here.
UPDATE sessions
   SET cost_basis = 'unpriced'
 WHERE cost_basis IN ('actual', 'projected')
   AND input_price_per_million_snapshot       = 0
   AND output_price_per_million_snapshot      = 0
   AND cache_read_price_per_million_snapshot  = 0
   AND cache_write_price_per_million_snapshot = 0;


-- ── STEP 2: plan-backed openai/* 'actual' → 'projected' ──────────────────────
--
-- Reclassify priced `openai/*` sessions still booked as 'actual' to 'projected',
-- but ONLY when durable install-wide credential evidence proves this deployment
-- could not have paid per-token for OpenAI:
--
--   • a (non-revoked) ChatGPT/Codex plan OAuth credential EXISTS
--     (key_name = '__OAUTH_CHATGPT_CODEX', provider_id = 'chatgpt_codex'), AND
--   • NO (non-revoked) 'OPENAI_API_KEY' credential exists anywhere.
--
-- The `sessions` schema has no per-row credential linkage, so the gate is
-- install-wide and all-or-nothing — matching the conservative stance of
-- migrations 84 and 89. On a deployment that mixed a real OpenAI API key with a
-- Codex plan, the gate does nothing (the ambiguity cannot be resolved from
-- stored data) and openai rows stay 'actual'. This makes the migration safe to
-- ship to any deployment: only installs that provably ran openai on a plan are
-- rewritten. Zero-priced openai rows are already 'unpriced' from step 1.
UPDATE sessions
   SET cost_basis = 'projected'
 WHERE cost_basis = 'actual'
   AND model_id LIKE 'openai/%'
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
-- (1) Cost-basis distribution before/after:
-- SELECT cost_basis, COUNT(*) AS sessions, COALESCE(SUM(cost_usd), 0) AS total_usd
--   FROM sessions GROUP BY cost_basis ORDER BY cost_basis;
--
-- (2) Install-gate state — does this install qualify for step 2 at all?
-- SELECT
--   EXISTS (SELECT 1 FROM credentials WHERE key_name = '__OAUTH_CHATGPT_CODEX'
--             AND revoked_at IS NULL) AS has_codex_plan_oauth,
--   EXISTS (SELECT 1 FROM credentials WHERE key_name = 'OPENAI_API_KEY'
--             AND revoked_at IS NULL) AS has_openai_api_key;
-- Qualifying install: (true, false). Any other combination reclassifies ZERO
-- rows in step 2.
--
-- (3) Zero-priced rows targeted by step 1 (run BEFORE; must be 0 AFTER):
-- SELECT COUNT(*) FROM sessions
--  WHERE cost_basis IN ('actual', 'projected')
--    AND input_price_per_million_snapshot = 0
--    AND output_price_per_million_snapshot = 0
--    AND cache_read_price_per_million_snapshot = 0
--    AND cache_write_price_per_million_snapshot = 0;
--
-- (4) Residual priced openai 'actual' AFTER (0 for a qualifying install):
-- SELECT COUNT(*), COALESCE(SUM(cost_usd), 0) FROM sessions
--  WHERE cost_basis = 'actual' AND model_id LIKE 'openai/%';
--
-- ROLLBACK
--   Neither step is auto-reversible from stored data alone (step 1 collapses two
--   source states into 'unpriced'). Do NOT drop the column. To reverse step 2
--   only, run the same predicate with the cost_basis filter inverted
--   (SET cost_basis='actual' WHERE cost_basis='projected' AND model_id LIKE
--   'openai/%' AND <same credential gate>).
