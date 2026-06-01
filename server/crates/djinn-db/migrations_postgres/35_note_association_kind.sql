-- F5: typed provenance edge on note_associations.
--
-- Adds a `kind` column to classify the relationship an association records.
-- The default `'co_access'` preserves the original Hebbian co-access semantics
-- (ADR-023) for every existing row and for the unqualified upsert paths.
--
-- The `'derived_from'` value records provenance: a note that was created from
-- another (e.g. a pattern consolidated from a cluster of cases) links back to
-- its source(s) with a `derived_from` edge. This is additive — existing
-- queries that do not select `kind` are unaffected, and the column carries a
-- NOT NULL default so historical rows need no backfill.
ALTER TABLE note_associations
    ADD COLUMN IF NOT EXISTS kind VARCHAR(32) NOT NULL DEFAULT 'co_access';

CREATE INDEX IF NOT EXISTS idx_note_associations_kind ON note_associations(kind);
