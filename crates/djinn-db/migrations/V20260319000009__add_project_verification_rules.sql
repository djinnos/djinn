-- Add verification_rules JSON column to projects.
-- Each rule is a { match_pattern: string, commands: [string] } object.
-- Stored as a JSON array; defaults to empty (no rules = fall back to full-project verification).
ALTER TABLE projects ADD COLUMN verification_rules TEXT NOT NULL DEFAULT '[]';
