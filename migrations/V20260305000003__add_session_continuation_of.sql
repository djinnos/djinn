-- Add continuation_of column to sessions table.
--
-- Links a compaction-triggered continuation session to its predecessor,
-- forming a chain that the UI can group into a single logical session timeline.
-- Reference: ADR-018 (Session Continuity Option C).

ALTER TABLE sessions ADD COLUMN continuation_of TEXT REFERENCES sessions(id);

CREATE INDEX idx_sessions_continuation_of ON sessions(continuation_of);
