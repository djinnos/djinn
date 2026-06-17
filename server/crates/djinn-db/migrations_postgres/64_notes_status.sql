-- Note lifecycle status substrate (ozz9 iy3l).
--
-- Single status column shared by active/archived/deprecated-like vocabulary.
-- Defaults existing and new notes to 'active'. This is the substrate the
-- memory-lifecycle sweep epics (yk9t) filter on; sweep actions must never
-- decay/archive a note that a human has moved out of 'active'.
ALTER TABLE notes
    ADD COLUMN IF NOT EXISTS status VARCHAR(32) NOT NULL DEFAULT 'active';

UPDATE notes SET status = 'active' WHERE status IS NULL OR status = '';

CREATE INDEX IF NOT EXISTS notes_status ON notes(status);
