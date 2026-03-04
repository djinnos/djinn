-- Add setup_commands and verification_commands JSON arrays to projects.
ALTER TABLE projects ADD COLUMN setup_commands        TEXT NOT NULL DEFAULT '[]';
ALTER TABLE projects ADD COLUMN verification_commands TEXT NOT NULL DEFAULT '[]';
