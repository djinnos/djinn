-- ao5x Wave 1 / Task 1: provenance-ready note_associations substrate.
--
-- Extends `note_associations` so multiple rows can exist for the same
-- canonical note pair, distinguished by (kind, source).
--
-- Changes:
--   1. Add `source` column (NOT NULL DEFAULT 'session_co_access') —
--      backfills every existing row as a session co-access substrate row.
--   2. Add embedding-provenance columns: `confidence`, `algorithm_version`,
--      `embedding_model`, `embedding_dim`, `last_refreshed_at`.
--   3. Widen the kind CHECK constraint to include 'authored' and
--      'embedding_related' alongside the existing semantic/provenance kinds.
--   4. Replace the pair-only PRIMARY KEY with a four-column key on
--      (note_a_id, note_b_id, kind, source) for idempotent upserts.
--
-- Canonical pair invariant (note_a_id < note_b_id) is preserved by the
-- existing CHECK constraint `chk_note_association_order`.

-- ── 1. Add `source` column with backfill default ─────────────────────────
-- Existing rows receive source = 'session_co_access' automatically via the
-- DEFAULT, so the Hebbian co-access substrate is preserved without a
-- separate UPDATE pass.
ALTER TABLE note_associations
    ADD COLUMN IF NOT EXISTS source VARCHAR(64) NOT NULL DEFAULT 'session_co_access';

-- ── 2. Add embedding-provenance columns ──────────────────────────────────
-- Nullable: only populated for embedding_related edges and future
-- provenance-rich upserts. Legacy co-access and semantic rows leave them
-- NULL.
ALTER TABLE note_associations
    ADD COLUMN IF NOT EXISTS confidence DOUBLE PRECISION NULL;

ALTER TABLE note_associations
    ADD COLUMN IF NOT EXISTS algorithm_version VARCHAR(64) NULL;

ALTER TABLE note_associations
    ADD COLUMN IF NOT EXISTS embedding_model VARCHAR(255) NULL;

ALTER TABLE note_associations
    ADD COLUMN IF NOT EXISTS embedding_dim INT NULL;

ALTER TABLE note_associations
    ADD COLUMN IF NOT EXISTS last_refreshed_at VARCHAR(64) NULL;

-- ── 3. Widen the allowed kind set ────────────────────────────────────────
-- Adds 'authored' (manual/human-authored edges) and 'embedding_related'
-- (edges derived from embedding similarity) to the existing semantic and
-- provenance kinds. Applied idempotently: drop then re-add so re-runs are
-- safe.
ALTER TABLE note_associations
    DROP CONSTRAINT IF EXISTS chk_note_associations_kind;

ALTER TABLE note_associations
    ADD CONSTRAINT chk_note_associations_kind
    CHECK (kind IN (
        'co_access',
        'derived_from',
        'builds_on',
        'contradicts',
        'supersedes',
        'exemplifies',
        'authored',
        'embedding_related'
    ));

-- ── 4. Replace pair-only PRIMARY KEY with (pair, kind, source) ───────────
-- This allows the same canonical note pair to carry multiple rows by
-- (kind, source) — e.g. a co_access / session_co_access row AND an
-- embedding_related / embedding_similarity row coexist without conflict.
--
-- Drop the auto-generated PK constraint, then re-add with the wider key.
-- Because `source` was already backfilled in step 1, every existing row
-- has (note_a_id, note_b_id, 'co_access', 'session_co_access') — no
-- duplicates can exist, so the new PK is safe.
ALTER TABLE note_associations
    DROP CONSTRAINT note_associations_pkey;

ALTER TABLE note_associations
    ADD PRIMARY KEY (note_a_id, note_b_id, kind, source);

-- ── Indexes ──────────────────────────────────────────────────────────────
-- The PK index now covers (note_a_id, note_b_id, kind, source).
-- Retain the existing single-column indexes on note_a_id and note_b_id
-- for queries that traverse by endpoint without filtering by kind/source.
-- Add a source index for source-filtered queries.
CREATE INDEX IF NOT EXISTS idx_note_associations_source
    ON note_associations(source);
