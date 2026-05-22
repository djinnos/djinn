-- Fill in JSON-shaped defaults for `NOT NULL` JSONB columns that were
-- originally ported from SQLite as bare `NOT NULL` (no default). Pre-migration
-- writes filled these with `[]` in the repository code; the Postgres port now
-- carries a `DEFAULT '[]'::jsonb` so insert sites that omit these columns
-- (TaskRepository::create_in_project omits `labels` and `memory_refs`, and
-- all test helpers that stub `INSERT INTO projects` omit `verification_rules`)
-- still get a valid empty array.

ALTER TABLE projects
    ALTER COLUMN verification_rules SET DEFAULT '[]'::jsonb;

ALTER TABLE tasks
    ALTER COLUMN labels              SET DEFAULT '[]'::jsonb,
    ALTER COLUMN acceptance_criteria SET DEFAULT '[]'::jsonb,
    ALTER COLUMN memory_refs         SET DEFAULT '[]'::jsonb;

ALTER TABLE epics
    ALTER COLUMN memory_refs SET DEFAULT '[]'::jsonb;

-- `notes` also carries unconditional NOT NULL columns for the JSON
-- `tags` / `scope_paths` arrays and the Markdown `content` body. A few code
-- paths (notably `reindex_from_disk` when the frontmatter omits tags/scope)
-- rely on the default; giving them one here avoids the papercut.
ALTER TABLE notes
    ALTER COLUMN tags        SET DEFAULT '[]'::jsonb,
    ALTER COLUMN scope_paths SET DEFAULT '[]'::jsonb,
    ALTER COLUMN content     SET DEFAULT '';
