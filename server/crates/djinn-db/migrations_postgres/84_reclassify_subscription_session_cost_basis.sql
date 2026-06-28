-- ──────────────────────────────────────────────────────────────────────────────
-- 84: Reclassify historical subscription / coding-plan sessions from 'actual' to
--     'projected' where durable stored evidence supports it.
-- ──────────────────────────────────────────────────────────────────────────────
--
-- CONTEXT
--   Migration 83 added `sessions.cost_basis` and conservatively backfilled EVERY
--   priced row as 'actual' (see its own comments). That default is correct for
--   API-key / pay-as-you-go providers but WRONG for personal subscription and
--   coding/token-plan providers whose list-rate cost is a projection, not real
--   spend. This migration repairs those historical rows where the durable stored
--   evidence — the `model_id` string — marks the session as subscription-backed.
--
--   The forward runtime fix (sibling task xrzj) added `CostBasisHint` so NEW
--   sessions are classified correctly at creation. This migration backfills the
--   pre-fix historical rows so usage analytics see a consistent cost basis.
--
-- IDEMPOTENCE GUARANTEE
--   The UPDATE's WHERE clause restricts to `cost_basis = 'actual'` AND a
--   subscription-evidence predicate. Once a matching row is rewritten to
--   'projected' it no longer satisfies `cost_basis = 'actual'`, so re-running
--   this migration updates zero rows. Rows already 'projected' or 'unpriced'
--   are never touched. The statement is therefore idempotent by predicate
--   design — no guard table, transaction flag, or idempotency key is required.
--
-- SCHEMA LIMITATION (no provider column)
--   `sessions` has NO dedicated provider_id column — only `model_id` which is
--   stored in the canonical `"<providerID>/<modelID>"` form produced at dispatch
--   time (see `CatalogService::find_model`, which `split_once('/')`s the stored
--   value, and `pick_any_default_model` / `pricing_for_all_models`, which build
--   it as `format!("{}/{}", provider.id, model.id)`).
--
--   The provider namespace is therefore DERIVED from the stored `model_id` as
--   the segment before the first '/' via `split_part(model_id, '/', 1)`. This
--   is a durable stored value (written at session creation from the resolved
--   catalog provider id), not a transient credential claim, so it is reliable
--   durable evidence for reclassification.
--
--   Limitation: if a future schema adds a dedicated provider column, this
--   derivation should be revisited. Canonical-alias matching (Rust's
--   `canonical_id`, which strips non-alphanumerics and lowercases) is NOT
--   reproduced here — catalog provider ids are already canonical lowercase, so
--   exact-lowercase matching against the builtin id set is sufficient. Any
--   historical row whose `model_id` prefix is a non-canonical alias would be
--   missed; that is acceptable (conservative) for a backfill.
--
-- RUST RULE ALIGNMENT (djinn-provider/src/catalog/builtin.rs)
--   The predicates below are a faithful SQL transcription of:
--     • `classify_provider(provider_id)` — builtin credential_class +
--       `is_subscription_id` fallback, and
--     • `governable_subscription_for_model("openai", model_id)` — the special
--       case where ChatGPT/Codex OAuth subscription models surface under the
--       `openai` namespace and are recognised by the `codex` model marker.
--
--   The sibling task sswh adds a Rust/SQL predicate drift guard to keep these
--   in sync as the builtin list evolves:
--     • Rust tests in `djinn-provider/src/catalog/builtin.rs::tests`
--       (drift_guard_sql_predicates_match_rust_classification,
--        drift_guard_builtin_subscription_ids_covered,
--        drift_guard_id_pattern_constants_match_migration).
--     • Migration-adjacent matrix: `djinn-db/docs/84-cost-basis-predicate-drift-guard.md`.
-- ──────────────────────────────────────────────────────────────────────────────


-- Helper view for readability: expose the derived provider namespace alongside
-- the raw model_id. Defined and dropped within this migration's conceptual
-- scope; validation queries below reference the same derivation inline.
--
-- (No persistent view is created — the derivation is inlined in the UPDATE so
-- the reclassification is self-contained and auditable in a single statement.)


UPDATE sessions
   SET cost_basis = 'projected'
 WHERE sessions.cost_basis = 'actual'
   AND (
        -- ── Predicate Family A: built-in subscription providers ───────────────
        -- Mirrors BUILTIN_PROVIDERS entries whose `credential_class` is
        -- `CredentialClass::Subscription`. These are the hand-registered
        -- subscription providers; their explicit class is authoritative.
        lower(split_part(model_id, '/', 1)) IN (
            'minimax-coding-plan',
            'xiaomi-token-plan-sgp',
            'kimi-for-coding',
            'opencode',
            'zai-coding-plan',
            'chatgpt_codex',
            'githubcopilot'
        )

        -- ── Predicate Family B: OpenAI Codex models (governable subscription) ─
        -- Mirrors `governable_subscription_for_model`: when the provider
        -- namespace is `openai` and the model id contains the `codex` marker
        -- (case-insensitive), the session is attributed to the ChatGPT/Codex
        -- OAuth subscription and is projected.
        --
        -- CRITICAL: plain `openai/<non-codex>` rows do NOT match this family.
        -- Ambiguous OpenAI rows without an explicit Codex marker remain
        -- 'actual' (a bare OpenAI BYO API key is a fungible metered key, never
        -- a subscription). See `governable_subscription_for_model` which
        -- returns None for `openai/gpt-5.5` etc.
        OR (
               lower(split_part(model_id, '/', 1)) = 'openai'
           AND lower(model_id) LIKE '%codex%'
        )

        -- ── Predicate Family C: id-pattern subscription fallback ──────────────
        -- Mirrors `is_subscription_id` — the fallback arm of
        -- `classify_provider` for models.dev-native providers that are NOT
        -- hand-registered as builtins (regional / plan-tier variants such as
        -- `xiaomi-token-plan-cn`, `zhipuai-coding-plan`,
        -- `alibaba-qwen-coding-plan`, …).
        --
        -- C1. Coding/token-plan / for-coding SEGMENTS (substring contains, as
        --     in Rust `id.contains("-coding-plan")` etc.):
        OR lower(split_part(model_id, '/', 1)) LIKE '%-coding-plan%'
        OR lower(split_part(model_id, '/', 1)) LIKE '%-token-plan%'
        OR lower(split_part(model_id, '/', 1)) LIKE '%-for-coding%'

        -- C2. Known consumer-subscription vendor PREFIXES (Rust `starts_with`):
        OR lower(split_part(model_id, '/', 1)) LIKE 'xiaomi%'
        OR lower(split_part(model_id, '/', 1)) LIKE 'moonshotai%'
        OR lower(split_part(model_id, '/', 1)) LIKE 'alibaba%'
        OR lower(split_part(model_id, '/', 1)) LIKE 'tencent%'
        OR lower(split_part(model_id, '/', 1)) LIKE 'stepfun%'
        OR lower(split_part(model_id, '/', 1)) LIKE 'kuae-cloud%'
        OR lower(split_part(model_id, '/', 1)) LIKE 'umans-ai%'

        -- C3. Known subscription EXACT ids (Rust `==`). Some overlap with the
        --     builtin set above (kimi-for-coding, minimax-coding-plan, opencode)
        --     — harmless; the full Rust SUBSCRIPTION_IDS list is reproduced for
        --     drift-resistance as the builtin list evolves.
        OR lower(split_part(model_id, '/', 1)) IN (
            'zai',
            'zhipuai',
            'kimi-for-coding',
            'minimax-coding-plan',
            'opencode',
            'opencode-go'
        )
   );


-- ══════════════════════════════════════════════════════════════════════════════
-- VALIDATION / OBSERVABILITY QUERIES  (NOT auto-executed)
-- ══════════════════════════════════════════════════════════════════════════════
-- Run these manually before AND after applying the UPDATE to confirm the
-- reclassification behaved as expected. Each block is independent. Prefix with
-- `\echo` or run in psql / your SQL client. Lines are commented (-- ) so the
-- migration runner does not execute them.
--
-- ── (1) Pre/post cost-basis COUNTS ──────────────────────────────────────────
-- SELECT cost_basis, COUNT(*) AS sessions
--   FROM sessions
--  GROUP BY cost_basis
--  ORDER BY cost_basis;
-- Expectation AFTER: 'projected' count rises by the number of reclassified
-- rows; 'actual' count drops by the same; 'unpriced' is unchanged.
--
--
-- ── (2) Summed USD by provider / cost_basis ────────────────────────────────
-- The provider namespace is derived from model_id (no provider column exists).
-- SELECT lower(split_part(model_id, '/', 1)) AS provider,
--        cost_basis,
--        COUNT(*)                            AS sessions,
--        COALESCE(SUM(cost_usd), 0)          AS total_usd
--   FROM sessions
--  GROUP BY provider, cost_basis
--  HAVING COUNT(*) > 0
--  ORDER BY provider, cost_basis;
-- Expectation AFTER: subscription providers (zai-coding-plan, minimax-…, etc.)
-- move their USD totals from 'actual' to 'projected'. Plain openai non-codex
-- USD stays under 'actual'.
--
--
-- ── (3) Predicate-family MATCH COUNTS (rows that WERE reclassified) ─────────
-- Run BEFORE to predict volume; the same query AFTER should return 0 because
-- all matches are now 'projected' (proves idempotence on the UPDATE itself).
-- SELECT
--   COUNT(*) FILTER (
--     WHERE lower(split_part(model_id, '/', 1)) IN
--           ('minimax-coding-plan','xiaomi-token-plan-sgp','kimi-for-coding',
--            'opencode','zai-coding-plan','chatgpt_codex','githubcopilot')
--   ) AS family_a_builtin_subscription,
--   COUNT(*) FILTER (
--     WHERE lower(split_part(model_id, '/', 1)) = 'openai'
--       AND lower(model_id) LIKE '%codex%'
--   ) AS family_b_openai_codex,
--   COUNT(*) FILTER (
--     WHERE lower(split_part(model_id, '/', 1)) LIKE '%-coding-plan%'
--        OR lower(split_part(model_id, '/', 1)) LIKE '%-token-plan%'
--        OR lower(split_part(model_id, '/', 1)) LIKE '%-for-coding%'
--        OR lower(split_part(model_id, '/', 1)) LIKE 'xiaomi%'
--        OR lower(split_part(model_id, '/', 1)) LIKE 'moonshotai%'
--        OR lower(split_part(model_id, '/', 1)) LIKE 'alibaba%'
--        OR lower(split_part(model_id, '/', 1)) LIKE 'tencent%'
--        OR lower(split_part(model_id, '/', 1)) LIKE 'stepfun%'
--        OR lower(split_part(model_id, '/', 1)) LIKE 'kuae-cloud%'
--        OR lower(split_part(model_id, '/', 1)) LIKE 'umans-ai%'
--        OR lower(split_part(model_id, '/', 1)) IN
--           ('zai','zhipuai','kimi-for-coding','minimax-coding-plan',
--            'opencode','opencode-go')
--   ) AS family_c_id_pattern_fallback
--   FROM sessions
--  WHERE cost_basis = 'actual';
-- NOTE: families can overlap for a given row (a row may match A and C); the
-- UPDATE applies each match once. The three counts are independent tallies,
-- not a partition.
--
--
-- ── (4) Residual AMBIGUOUS OpenAI totals (must remain 'actual') ─────────────
-- Confirms plain openai rows WITHOUT a codex marker were NOT reclassified.
-- SELECT COUNT(*)                   AS residual_openai_actual,
--        COALESCE(SUM(cost_usd), 0) AS residual_openai_actual_usd
--   FROM sessions
--  WHERE cost_basis = 'actual'
--    AND lower(split_part(model_id, '/', 1)) = 'openai'
--    AND lower(model_id) NOT LIKE '%codex%';
-- Expectation: this is unchanged by the migration (the only openai rows moved
-- to 'projected' are those WITH a codex marker).
--
--
-- ── (5) SAMPLE rewritten rows (inspect before/after) ────────────────────────
-- BEFORE (the rows about to change):
--   SELECT id, model_id, cost_basis, cost_usd, tokens_in, tokens_out
--     FROM sessions
--    WHERE cost_basis = 'actual'
--      AND ( /* …full predicate from the UPDATE… */ )
--    LIMIT 50;
-- AFTER (confirm they are now projected):
--   SELECT id, model_id, cost_basis, cost_usd
--     FROM sessions
--    WHERE cost_basis = 'projected'
--      AND ( /* …same predicate… */ )
--    LIMIT 50;
--
--
-- ── (6) IDEMPOTENCE check ───────────────────────────────────────────────────
-- Re-run the migration (or its UPDATE) and confirm zero rows are affected:
--   UPDATE sessions SET cost_basis = 'projected' WHERE … ;  -- should report
--   UPDATE 0 on the second run.
-- Equivalently, the Family-match count in (3) run AFTER the migration must be 0.
--
--
-- ── (7) BATCHING / INDEX strategy ──────────────────────────────────────────
-- The UPDATE filters on `cost_basis = 'actual'`, which has no dedicated index
-- (migration 83 added the column but no partial index). It therefore performs a
-- sequential scan of the sessions table — acceptable for the typical sessions
-- volume (thousands to low-millions of rows).
--
-- For environments with very large session tables where a single statement
-- would hold locks / WAL too long, apply the SAME predicate in bounded batches
-- keyed by a stable range, e.g.:
--
--   SET LOCAL statement_timeout = '30s';
--   UPDATE sessions SET cost_basis = 'projected'
--    WHERE ctid IN (
--      SELECT ctid FROM sessions
--       WHERE cost_basis = 'actual' AND ( /* …full predicate… */ )
--       ORDER BY started_at
--       LIMIT 50000
--    );
-- Repeat until 0 rows are affected. Because the predicate is idempotent
-- (it only matches cost_basis = 'actual'), batched partial runs converge to the
-- same final state as the single statement above.
--
-- A supporting partial index MAY be added to speed the scan and future audits:
--   CREATE INDEX IF NOT EXISTS idx_sessions_cost_basis_actual
--       ON sessions(started_at) WHERE cost_basis = 'actual';
-- (Not created by this migration to avoid schema churn; add operationally if
-- the row count warrants it.)
--
--
-- ── (8) ROLLBACK / forward-fix guidance ─────────────────────────────────────
-- This is a one-time, conservative backfill aligned with the forward runtime
-- fix (task xrzj) and the predicate drift guard (task sswh).
--
-- To UNDO this specific reclassification (restore reclassified rows to actual),
-- run the SAME predicate but invert the cost_basis filter — this reverses the
-- migration exactly because the predicate set is unchanged:
--
--   UPDATE sessions
--      SET cost_basis = 'actual'
--    WHERE sessions.cost_basis = 'projected'
--      AND ( /* …identical predicate family A | B | C from the UPDATE above… */ );
--
-- This restores only the rows this migration rewrote (subscription-evidence
-- rows that were originally 'actual'); it does not touch rows that were
-- legitimately 'projected' from the forward fix for sessions created after 84.
-- To distinguish, additionally scope the rollback to rows created before this
-- migration's application timestamp if a precise restore is required.
--
-- Do NOT roll back the column itself — cost_basis remains (migration 83 owns
-- the column). Rolling back only reverses the value rewrite performed here.
