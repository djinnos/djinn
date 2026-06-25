-- Add per-session cost-basis column distinguishing actual API spend from
-- projected subscription-equivalent cost and unpriced sessions.
--
-- Values:
--   'actual'    — API-key / pay-as-you-go provider; list-rate cost is real spend.
--   'projected' — subscription / coding-plan provider; list-rate cost is a
--                  projection and actual API spend is $0.
--   'unpriced'  — uncatalogued or missing-price session; excluded from both
--                  actual and projected dollar aggregates, but counted visibly.
--
-- Backfill strategy for existing rows:
--   Rows with at least one non-null price snapshot column are assumed to be
--   pay-as-you-go (API-key) sessions and set to 'actual'.  This is a safe
--   default: the vast majority of priced historical rows were from API-key
--   providers (Anthropic, OpenAI, Google, etc.).  If more-precise retroactive
--   classification is needed later, a backfill pass keyed on the credential
--   owner can re-classify specific rows to 'projected'.
--   Rows with all snapshot columns NULL (no pricing data) are set to 'unpriced'.
--
-- No credential foreign key is introduced; the basis is a denormalized label
-- derived at session creation from the resolved catalog pricing + credential
-- class.

ALTER TABLE sessions
    ADD COLUMN cost_basis TEXT NOT NULL DEFAULT 'unpriced'
        CONSTRAINT sessions_cost_basis_check
            CHECK (cost_basis IN ('actual', 'projected', 'unpriced'));

-- Backfill: priced rows → 'actual', unpriced rows remain 'unpriced' (the default).
UPDATE sessions
   SET cost_basis = 'actual'
 WHERE input_price_per_million_snapshot IS NOT NULL
    OR output_price_per_million_snapshot IS NOT NULL
    OR cache_read_price_per_million_snapshot IS NOT NULL
    OR cache_write_price_per_million_snapshot IS NOT NULL;
