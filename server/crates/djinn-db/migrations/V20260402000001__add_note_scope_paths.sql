-- Add scope_paths column for path-scoped knowledge injection.
-- JSON array of relative path prefixes where this note applies.
-- Empty array '[]' = global note (injected everywhere).
-- Example: '["server/crates/djinn-db", "server/crates/djinn-agent"]'
ALTER TABLE notes ADD COLUMN scope_paths TEXT NOT NULL DEFAULT '[]';
