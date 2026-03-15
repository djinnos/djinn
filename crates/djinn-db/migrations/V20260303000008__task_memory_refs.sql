-- Add memory_refs column to tasks for bidirectional note-task linking.
ALTER TABLE tasks ADD COLUMN memory_refs TEXT NOT NULL DEFAULT '[]';
