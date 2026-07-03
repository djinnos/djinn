-- Migration 92: Durable compaction boundary storage.
--
-- The two-phase compaction contract requires durable records that distinguish
-- a `started` entry (compaction underway, no summary yet) from a completed
-- `ended` record (accepted summary + retained-tail identity available).
-- `load_conversation` projection (owned by a later task) will consult only
-- `ended` rows; `started` rows left behind by a crash, summarizer error,
-- or kill are ignored.
--
-- This migration is additive: no existing `session_messages` rows are
-- rewritten or deleted.
--
-- Columns:
--   * `id`                       — boundary row identity (UUIDv7).
--   * `session_id`               — owning session (FK to `sessions(id)`).
--   * `phase`                    — `started` or `ended`.
--   * `schema_version`           — version of the compaction payload schema
--                                  (integer, default 1) so projection can
--                                  evolve safely.
--   * `first_message_id`         — first `session_messages.id` in the
--                                  compacted source range.
--   * `last_compacted_message_id`— last source message whose content was
--                                  folded into the summary.
--   * `first_retained_message_id`— first raw message that survives after
--                                  compaction (start of the retained tail).
--   * `retained_tail_hash`       — stable content hash of the retained tail
--                                  (e.g. `sha256:<hex>`), used to detect
--                                  drift between boundary record and actual
--                                  tail at projection time.
--   * `summary_text`             — the accepted compaction summary; NULL on
--                                  `started`, populated on `ended`.
--   * `marker_metadata`          — JSONB blob for projection markers and
--                                  other metadata carried from the compaction
--                                  package (e.g. token counts, marker kind).
--   * `created_at`               — row creation timestamp (ISO-8601 UTC).
--   * `completed_at`             — set when phase transitions to `ended`;
--                                  NULL while `started`.
--
-- Indexes back the two most important query paths:
--   1. Latest completed boundary per session.
--   2. Boundary count per session.

CREATE TABLE IF NOT EXISTS session_compaction_boundaries (
    id                          VARCHAR(36)  NOT NULL PRIMARY KEY,
    session_id                  VARCHAR(36)  NOT NULL,
    phase                       VARCHAR(32)  NOT NULL DEFAULT 'started',
    schema_version              INTEGER      NOT NULL DEFAULT 1,
    first_message_id            VARCHAR(36)  NULL,
    last_compacted_message_id   VARCHAR(36)  NULL,
    first_retained_message_id   VARCHAR(36)  NULL,
    retained_tail_hash          TEXT         NULL,
    summary_text                TEXT         NULL,
    marker_metadata             JSONB        NULL,
    created_at                  VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    completed_at                VARCHAR(64)  NULL,
    CONSTRAINT fk_session_compaction_boundaries_session
        FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

-- Latest completed boundary per session (ended phase, ordered newest-first).
CREATE INDEX idx_scb_session_completed
    ON session_compaction_boundaries(session_id, completed_at DESC)
    WHERE phase = 'ended';

-- All boundaries per session for counting and iteration.
CREATE INDEX idx_scb_session_created
    ON session_compaction_boundaries(session_id, created_at DESC);
