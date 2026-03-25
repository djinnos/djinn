-- Add storage discriminator for note backing store.
ALTER TABLE notes ADD COLUMN storage TEXT NOT NULL DEFAULT 'file';
