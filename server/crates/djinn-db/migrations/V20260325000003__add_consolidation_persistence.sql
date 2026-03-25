-- Persist durable provenance for consolidated notes and run metrics.
CREATE TABLE consolidated_note_provenance (
    note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (note_id, session_id)
);

CREATE INDEX idx_consolidated_note_provenance_session_id
    ON consolidated_note_provenance(session_id);

CREATE TABLE consolidation_run_metrics (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    note_type TEXT NOT NULL,
    status TEXT NOT NULL,
    scanned_note_count INTEGER NOT NULL,
    candidate_cluster_count INTEGER NOT NULL,
    consolidated_cluster_count INTEGER NOT NULL,
    consolidated_note_count INTEGER NOT NULL,
    source_note_count INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    error_message TEXT
);

CREATE INDEX idx_consolidation_run_metrics_project_note_type_started_at
    ON consolidation_run_metrics(project_id, note_type, started_at DESC);
