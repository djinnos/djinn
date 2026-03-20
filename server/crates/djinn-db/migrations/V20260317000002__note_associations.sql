-- Note associations table for Hebbian co-access learning.
-- Implicit co-access relationships between notes are recorded here
-- and used by the retrieval pipeline (ADR-023).

CREATE TABLE note_associations (
    note_a_id       TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    note_b_id       TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    weight          REAL NOT NULL DEFAULT 0.01,
    co_access_count INTEGER NOT NULL DEFAULT 1,
    last_co_access  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (note_a_id, note_b_id),
    CHECK (note_a_id < note_b_id)  -- canonical ordering prevents duplicates
);

-- Indexes for association queries
CREATE INDEX idx_note_associations_a ON note_associations(note_a_id);
CREATE INDEX idx_note_associations_b ON note_associations(note_b_id);
CREATE INDEX idx_note_associations_weight ON note_associations(weight);
