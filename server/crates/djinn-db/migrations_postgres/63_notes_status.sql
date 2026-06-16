ALTER TABLE notes
    ADD COLUMN IF NOT EXISTS status VARCHAR(32) NOT NULL DEFAULT 'active';

UPDATE notes SET status = 'active' WHERE status IS NULL OR status = '';

CREATE INDEX IF NOT EXISTS notes_status ON notes(status);
