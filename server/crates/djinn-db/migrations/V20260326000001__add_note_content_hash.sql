-- Add content hash support for deterministic housekeeping/backfill.
ALTER TABLE notes ADD COLUMN content_hash TEXT;
CREATE INDEX notes_project_content_hash_idx ON notes(project_id, content_hash);
