-- Session cost basis: classifies each session's dollar figure as actual API
-- spend, projected subscription-equivalent cost, or unpriced (excluded).
--
-- `cost_usd` is preserved as the list-rate / projected-equivalent value.
-- `cost_basis` tells downstream analytics which sessions contribute to real
-- API spend versus subscription projection estimates.
--
-- Backfill strategy:
--   - Rows with `cost_usd IS NOT NULL` → `'actual'` (safe historical default;
--     at the time migration 66 landed, only API-key providers were priced).
--   - Rows with `cost_usd IS NULL` → `'unpriced'` (uncatalogued or
--     missing-price sessions).

ALTER TABLE sessions
    ADD COLUMN cost_basis TEXT NOT NULL DEFAULT 'unpriced'
    CHECK (cost_basis IN ('actual', 'projected', 'unpriced'));

-- Backfill existing rows.
UPDATE sessions SET cost_basis = 'actual'   WHERE cost_usd IS NOT NULL;
UPDATE sessions SET cost_basis = 'unpriced' WHERE cost_usd IS NULL;

-- Drop the default so every INSERT must provide a value.
ALTER TABLE sessions ALTER COLUMN cost_basis DROP DEFAULT;
