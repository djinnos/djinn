-- Fill in JSON-shaped defaults for `NOT NULL` LONGTEXT columns that were
-- originally ported from SQLite as bare `NOT NULL` (no default). Pre-migration
-- sqlite writes filled these with `'[]'` in the repository code; the
-- MySQL/Dolt ports miss some of those columns at insert time (for example
-- `TaskRepository::create_in_project` omits `labels` and `memory_refs`,
-- and all test helpers that stub `INSERT INTO projects` omit
-- `verification_rules`).
--
-- Giving these columns a `DEFAULT ('[]')` keeps the schema strict for readers
-- (`SELECT labels` still returns a JSON array string) while removing the
-- last-mile papercut of having to spell the JSON literal on every insert.
--
-- `verification_rules` is stored as a JSON array; all other columns below are
-- JSON arrays as well.

ALTER TABLE projects
    MODIFY COLUMN verification_rules LONGTEXT NOT NULL DEFAULT ('[]');

ALTER TABLE tasks
    MODIFY COLUMN labels              LONGTEXT NOT NULL DEFAULT ('[]'),
    MODIFY COLUMN acceptance_criteria LONGTEXT NOT NULL DEFAULT ('[]'),
    MODIFY COLUMN memory_refs         LONGTEXT NOT NULL DEFAULT ('[]');

ALTER TABLE epics
    MODIFY COLUMN memory_refs LONGTEXT NOT NULL DEFAULT ('[]');

-- `notes` also carries unconditional NOT NULL LONGTEXT columns for the JSON
-- `tags` / `scope_paths` arrays and the Markdown `content` body. These come
-- from the SQLite-era schema, where the repository layer always filled them.
-- A few code paths (notably `reindex_from_disk` when the frontmatter omits
-- tags/scope) rely on the default; giving them one here avoids the papercut.
ALTER TABLE notes
    MODIFY COLUMN tags        LONGTEXT NOT NULL DEFAULT ('[]'),
    MODIFY COLUMN scope_paths LONGTEXT NOT NULL DEFAULT ('[]'),
    MODIFY COLUMN content     LONGTEXT NOT NULL DEFAULT ('');
