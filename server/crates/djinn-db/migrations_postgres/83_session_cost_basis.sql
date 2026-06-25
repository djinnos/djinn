-- Session cost basis: classifies whether `cost_usd` represents real API spend,
-- a projected subscription-equivalent cost, or is unpriced.
--
-- Values:
--   'actual'    — API-key provider sessions where `cost_usd` is real spend.
--   'projected' — subscription/coding-plan sessions where `cost_usd` is a
--                  list-rate projection and actual API spend is $0.
--   'unpriced'  — uncatalogued or missing-price sessions; excluded from both
--                  actual and projected dollar totals.
--
-- Backfill strategy:
--   Rows with `cost_usd IS NOT NULL` are priced historical sessions. At the
--   time migration 66 landed, only API-key providers were priced (subscription
--   pricing snapshooting was not yet wired). Safe historical default: 'actual'.
--   Rows with `cost_usd IS NULL` are unpriced: 'unpriced'.
--
-- No credential foreign key is introduced; basis is derived at session creation
-- from the catalog's provider classification and pricing availability.
ALTER TABLE sessions
    ADD COLUMN cost_basis TEXT NOT NULL DEFAULT 'unpriced'
    CHECK (cost_basis IN ('actual', 'projected', 'unpriced'));

-- Backfill priced historical rows as 'actual' (see rationale above).
UPDATE sessions SET cost_basis = 'actual' WHERE cost_usd IS NOT NULL;

-- Remove the column default so every INSERT must explicitly provide a value.
ALTER TABLE sessions ALTER COLUMN cost_basis DROP DEFAULT;
