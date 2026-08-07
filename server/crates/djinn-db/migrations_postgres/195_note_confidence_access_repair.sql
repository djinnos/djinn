-- Migration 195: separate retrieval utility from epistemic confidence and open
-- a clean, invocation-keyed note-access accounting era (proposal 9xih).
--
-- DEPLOYMENT-NEUTRAL BY CONSTRUCTION. Every statement below is a predicate over
-- a whole table. No project id, note id, session id, task run id, or "our data
-- looks like X" assumption appears anywhere, so this is correct for any
-- operator's database — including a brand-new one where migration 1 just
-- created `notes` empty and every UPDATE matches zero rows.
--
-- Every statement is also idempotent: re-running the whole file leaves the
-- database in the identical state, and does so regardless of how many rows
-- happen to exist.

-- ── 1. Confidence ceiling ───────────────────────────────────────────────────
--
-- `notes.confidence` defaulted to 1.0 while the Bayesian `CONFIDENCE_CEILING`
-- is 0.975. `bayesian_update` clamps to that ceiling, so applying a POSITIVE
-- epistemic signal (`USER_CONFIRM`) to an untouched 1.0 note *lowered* it to
-- 0.975 — below every untouched peer in confidence ordering. Confirming a note
-- demoted it. Normalizing the default and the exact-1.0 rows to the ceiling
-- removes a distinction that was never evidence.
ALTER TABLE notes ALTER COLUMN confidence SET DEFAULT 0.975;

-- Only exactly-1.0 rows move. Any value below 1.0 is the posterior of a real
-- epistemic signal and is never rewritten. Re-running matches zero rows because
-- 0.975 <> 1.0.
UPDATE notes SET confidence = 0.975 WHERE confidence = 1.0;

-- ── 2. Legacy access rebase ─────────────────────────────────────────────────
--
-- Until now BOTH an explicit `memory_read` AND an ADR-054 `memory_search`
-- result display incremented `access_count` / `last_accessed`. The two are
-- indistinguishable in these last-write-wins scalars, so their provenance
-- cannot be reconstructed — only discarded. Rebase to a neutral, *defined*
-- baseline rather than a guess: count 0, and the note's own `created_at`.
--
-- `created_at` is chosen over NULL because `last_accessed` is NOT NULL and
-- feeds age/temporal scoring; it keeps a well-formed timestamp while asserting
-- no read ever happened. New notes already initialize the same way.
--
-- Re-running matches zero rows: after the first pass every row already
-- satisfies both equalities.
UPDATE notes
   SET access_count  = 0,
       last_accessed = created_at
 WHERE access_count <> 0
    OR last_accessed <> created_at;

-- ── 3. Invocation-keyed ledger era ──────────────────────────────────────────
--
-- `note_access_events` ALREADY EXISTS: migration 189 (proposal u46i AC6)
-- created it, and its rows are the join side of the shipped
-- `P(memory_read | Injected)` report. It is therefore ALTERed here, never
-- dropped and never recreated — deleting those rows would silently break a
-- shipped metric that guards a 30-day retention window.
--
-- The new era is empty all the same, because the new era is *defined* as the
-- rows carrying a non-null `invocation_id`, and this migration adds none.
-- Every pre-9xih row keeps `invocation_id IS NULL` and can never be mistaken
-- for a 9xih-era access event or satisfy a replay probe.
ALTER TABLE note_access_events
    ADD COLUMN IF NOT EXISTS invocation_id VARCHAR(64) NULL;

-- `(invocation_id, note_id)` is the replay key: a uniqueness conflict IS a
-- caller retry of one logical invocation and must NOT increment a counter.
--
-- Postgres treats NULLs as distinct in a unique index, so the arbitrarily many
-- legacy rows with `invocation_id IS NULL` coexist freely and index creation
-- cannot fail on existing data, however much of it there is.
CREATE UNIQUE INDEX IF NOT EXISTS uq_note_access_events_invocation_note
    ON note_access_events (invocation_id, note_id);
