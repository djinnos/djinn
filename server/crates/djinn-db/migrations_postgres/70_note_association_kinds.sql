-- vrn9 Wave 1: widen note_associations.kind to accept typed-edge values.
--
-- The F5 substrate (migration 35) added the `kind` column with a DEFAULT of
-- `'co_access'` and no CHECK constraint. This migration adds a CHECK constraint
-- that explicitly enumerates every allowed kind value:
--   co_access, derived_from, builds_on, contradicts, supersedes, exemplifies
--
-- The constraint is applied idempotently: we DROP any pre-existing constraint
-- of the same name before ADD-ing it so re-running the migration is safe.
ALTER TABLE note_associations
    DROP CONSTRAINT IF EXISTS chk_note_associations_kind;

ALTER TABLE note_associations
    ADD CONSTRAINT chk_note_associations_kind
    CHECK (kind IN ('co_access', 'derived_from', 'builds_on', 'contradicts', 'supersedes', 'exemplifies'));
