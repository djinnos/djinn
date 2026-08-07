-- Migration 196 (proposal t5rn): open and bound the note consolidation
-- lifecycle.
--
-- Every statement below is purely structural. Nothing here references a
-- particular project, session, note, or task, nothing assumes any row exists,
-- and every statement is a valid no-op on a brand-new empty database.
--
-- Three things are added:
--
--   1. Durable, unique consolidation-attempt identity carried on the immutable
--      canonical *creation* revision. This is the only safe retry witness: a
--      source note's `superseded` status can be produced by write-time dedup or
--      by a different consolidation set, so status alone must never be read as
--      evidence that a specific attempt completed.
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
ALTER TABLE note_revision_events
    ADD COLUMN IF NOT EXISTS consolidation_attempt_id CHAR(64) NULL;

-- The attempt identity may only ever be stamped on a system-attributed
-- `consolidation` creation revision. This keeps the retry witness on the one
-- immutable row that also carries the canonical's identity.
ALTER TABLE note_revision_events
    ADD CONSTRAINT chk_note_revision_events_consolidation_attempt
    CHECK (
        consolidation_attempt_id IS NULL
        OR (
            event_kind = 'created'
            AND actor_kind = 'system'
            AND subsystem = 'consolidation'
            AND note_id IS NOT NULL
        )
    );

-- Partial uniqueness leaves every pre-existing (and every non-consolidation)
-- revision row untouched while making one attempt identity unrepeatable. This
-- is the second defense behind the sorted `FOR UPDATE` source locks: two
-- concurrent runners that compute the same attempt identity cannot both commit.
CREATE UNIQUE INDEX IF NOT EXISTS note_revision_events_consolidation_attempt_unique
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
