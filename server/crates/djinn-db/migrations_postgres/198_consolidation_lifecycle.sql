-- Migration 198 (proposal t5rn): open and bound the note consolidation
-- lifecycle.
--
-- Every statement below is purely structural: nothing references a particular
-- project, session, note, or task, and nothing assumes any row exists. sqlx
-- applies each migration exactly once, so the `ADD CONSTRAINT` below is written
-- for a single application (Postgres has no `ADD CONSTRAINT IF NOT EXISTS`).
--
-- Three things are added:
--
--   1. Durable consolidation-attempt identity, unique on the canonical's
--      immutable *creation* revision and repeated (deliberately, non-uniquely)
--      on each retired source's *update* revision. Together those rows are the
--      only safe retry witness: a source note's `superseded` status can be
--      produced by write-time dedup or by a different consolidation set, so
--      status alone must never be read as evidence that a specific attempt
--      completed.
--
--      Safe on a populated database: the column is added by this same migration,
--      so every pre-existing row has NULL and satisfies the CHECK's first
--      disjunct trivially. `VALIDATE CONSTRAINT` therefore scans without any
--      possibility of failing on historical data.
--   2. A lookup index for the canonical-identity predicate. Canonical notes are
--      authoritatively identified by an immutable creation revision attributed
--      to the `consolidation` subsystem — never by a mutable tag, title,
--      permalink, content, or edge shape — so candidate selection needs that
--      predicate to be cheap.
--   3. Resumable state for the one-time source-provenance backfill.
--
-- `consolidated_note_provenance` already carries PRIMARY KEY
-- (note_id, session_id) from migration 1, which is exactly the uniqueness rule
-- both the extraction writer and the backfill rely on for their idempotent
-- `ON CONFLICT DO NOTHING` inserts. No change is needed there.

-- ── 1. Consolidation attempt identity ────────────────────────────────────────
--
-- SHA-256 hex digest of
--   version || project_id || session_id || note_type || sorted_source_ids
--          || canonical_body_digest
-- computed by the repository before the canonical transaction opens.
--
-- `VARCHAR(64)` rather than `CHAR(64)`: migration 27 deliberately moved hash
-- columns off blank-padded `bpchar`, and a fixed-width digest gains nothing
-- from the padding semantics.
ALTER TABLE note_revision_events
    ADD COLUMN IF NOT EXISTS consolidation_attempt_id VARCHAR(64) NULL;

-- The attempt identity is stamped on exactly two kinds of row, both immutable
-- and both written inside the one canonical transaction:
--
--   * the canonical's `created` revision — the attempt's identity, and
--   * each retired source's `updated` revision — the attempt's *directed*
--     source set.
--
-- The second is what makes the retry witness sound. `note_associations` is an
-- order-normalized, undirected substrate with no direction column, so reading
-- `supersedes` endpoints from it cannot distinguish "this canonical superseded
-- X" from "X superseded this canonical". Once a canonical is itself deprecated
-- with a superseding note (reachable today through extraction's
-- `DeprecateWithSupersedes`), an edge-derived endpoint set gains a spurious
-- member and a correctly committed attempt would be misreported as an invariant
-- violation. The ledger rows below are directed by construction and no later
-- edge can perturb them.
--
-- Written `NOT VALID` and validated separately so the deploy takes ACCESS
-- EXCLUSIVE only briefly instead of scanning the whole revision ledger under it.
ALTER TABLE note_revision_events
    ADD CONSTRAINT chk_note_revision_events_consolidation_attempt
    CHECK (
        consolidation_attempt_id IS NULL
        OR (
            event_kind IN ('created', 'updated')
            AND actor_kind = 'system'
            AND subsystem = 'consolidation'
            AND note_id IS NOT NULL
        )
    ) NOT VALID;

ALTER TABLE note_revision_events
    VALIDATE CONSTRAINT chk_note_revision_events_consolidation_attempt;

-- Partial uniqueness leaves every pre-existing (and every non-consolidation)
-- revision row untouched while making one attempt *identity* unrepeatable. It
-- is scoped to the `created` row because the retirement rows deliberately share
-- their attempt's identity. This is the second defense behind the sorted
-- `FOR UPDATE` source locks: two concurrent runners that compute the same
-- attempt identity cannot both commit.
CREATE UNIQUE INDEX IF NOT EXISTS note_revision_events_consolidation_attempt_unique
    ON note_revision_events(consolidation_attempt_id)
    WHERE consolidation_attempt_id IS NOT NULL AND event_kind = 'created';

-- Lookup support for recovering an attempt's directed source set.
CREATE INDEX IF NOT EXISTS note_revision_events_consolidation_attempt_lookup
    ON note_revision_events(consolidation_attempt_id)
    WHERE consolidation_attempt_id IS NOT NULL;

-- ── 2. Immutable canonical-attribution lookup ────────────────────────────────
--
-- Supports both the `NOT EXISTS (...)` source-eligibility predicate and the
-- attempt-identity retry lookup.
CREATE INDEX IF NOT EXISTS note_revision_events_consolidation_creation
    ON note_revision_events(note_id)
    WHERE event_kind = 'created' AND subsystem = 'consolidation';

-- ── 3. Resumable provenance backfill state ───────────────────────────────────
--
-- One row per backfill scope key. `last_note_id` is the exclusive resume
-- watermark: the next batch scans `notes.id > last_note_id` in ascending id
-- order, so an interrupted backfill never rescans a completed batch. The
-- counters are report-only; the backfill reports notes it could not seed rather
-- than guessing a session for them.
CREATE TABLE IF NOT EXISTS consolidation_provenance_backfill_state (
    scope_key                      VARCHAR(191) NOT NULL PRIMARY KEY,
    last_note_id                   VARCHAR(36)  NULL,
    completed                      BOOLEAN      NOT NULL DEFAULT FALSE,
    scanned_note_count             BIGINT       NOT NULL DEFAULT 0,
    seeded_provenance_row_count    BIGINT       NOT NULL DEFAULT 0,
    skipped_without_provenance     BIGINT       NOT NULL DEFAULT 0,
    skipped_canonical_attribution  BIGINT       NOT NULL DEFAULT 0,
    skipped_project_mismatch       BIGINT       NOT NULL DEFAULT 0,
    updated_at                     VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);
