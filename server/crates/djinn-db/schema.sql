-- Canonical SQLite schema snapshot for djinn-db.
-- This file reflects the fully migrated schema including notes cognitive columns
-- and note_associations table for Hebbian co-access learning.

CREATE TABLE settings (
    key        TEXT NOT NULL PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE projects (
    id         TEXT NOT NULL PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    path       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    target_branch TEXT NOT NULL DEFAULT 'main',
    auto_merge INTEGER NOT NULL DEFAULT 0,
    sync_enabled INTEGER NOT NULL DEFAULT 1,
    sync_remote TEXT
);

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'todo',
    issue_type TEXT NOT NULL DEFAULT 'task',
    parent_task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    priority INTEGER,
    labels TEXT NOT NULL DEFAULT '[]',
    assignee TEXT,
    batch_id TEXT,
    source_branch TEXT,
    merge_commit_sha TEXT,
    archived_at TEXT,
    blocked_reason TEXT,
    blocked_at TEXT,
    pm_override INTEGER NOT NULL DEFAULT 0,
    pm_override_reason TEXT,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_tasks_project_id ON tasks(project_id);
CREATE INDEX idx_tasks_project_status ON tasks(project_id, status);
CREATE INDEX idx_tasks_project_priority ON tasks(project_id, priority);
CREATE INDEX idx_tasks_project_parent ON tasks(project_id, parent_task_id);
CREATE INDEX idx_tasks_project_issue_type ON tasks(project_id, issue_type);
CREATE INDEX idx_tasks_project_archived ON tasks(project_id, archived_at);

CREATE TABLE task_blockers (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    blocker_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (task_id, blocker_task_id)
);

CREATE INDEX idx_task_blockers_blocker ON task_blockers(blocker_task_id);

CREATE TABLE task_activity_log (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    actor_role TEXT NOT NULL,
    actor_id TEXT,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    archived_at TEXT
);

CREATE INDEX idx_task_activity_project_task_created ON task_activity_log(project_id, task_id, created_at DESC);
CREATE INDEX idx_task_activity_project_created ON task_activity_log(project_id, created_at DESC);
CREATE INDEX idx_task_activity_archived ON task_activity_log(archived_at);

CREATE TABLE notes (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    permalink     TEXT NOT NULL,
    title         TEXT NOT NULL,
    file_path     TEXT NOT NULL,
    storage       TEXT NOT NULL DEFAULT 'file',
    note_type     TEXT NOT NULL,
    folder        TEXT NOT NULL,
    tags          TEXT NOT NULL,
    content       TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_accessed TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    access_count  INTEGER NOT NULL DEFAULT 0,
    confidence    REAL NOT NULL DEFAULT 1.0,
    abstract      TEXT,
    overview      TEXT,
    UNIQUE(project_id, permalink),
    UNIQUE(project_id, file_path)
);

CREATE INDEX idx_notes_project_folder ON notes(project_id, folder);
CREATE INDEX idx_notes_project_updated ON notes(project_id, updated_at DESC);
CREATE INDEX idx_notes_project_last_accessed ON notes(project_id, last_accessed DESC);

CREATE VIRTUAL TABLE notes_fts USING fts5(
    note_id UNINDEXED,
    title,
    content,
    tags,
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, note_id, title, content, tags)
    VALUES (new.rowid, new.id, new.title, new.content, new.tags);
END;

CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN
    DELETE FROM notes_fts WHERE rowid = old.rowid;
END;

CREATE TRIGGER notes_au AFTER UPDATE OF title, content, tags ON notes BEGIN
    DELETE FROM notes_fts WHERE rowid = old.rowid;
    INSERT INTO notes_fts(rowid, note_id, title, content, tags)
    VALUES (new.rowid, new.id, new.title, new.content, new.tags);
END;

CREATE TABLE note_links (
    source_note_id      TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    target_permalink_raw TEXT NOT NULL,
    target_note_id      TEXT REFERENCES notes(id) ON DELETE SET NULL,
    raw_text            TEXT NOT NULL,
    occurrence_index    INTEGER NOT NULL,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (source_note_id, occurrence_index)
);

CREATE INDEX idx_note_links_source ON note_links(source_note_id);
CREATE INDEX idx_note_links_target ON note_links(target_note_id);
CREATE INDEX idx_note_links_target_raw ON note_links(target_permalink_raw);

CREATE TABLE model_health (
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    status TEXT NOT NULL,
    latency_ms INTEGER,
    success_rate REAL,
    sample_size INTEGER,
    error_message TEXT,
    details TEXT,
    checked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (provider, model)
);

CREATE TABLE credentials (
    provider TEXT NOT NULL,
    project_id TEXT,
    encrypted_payload TEXT NOT NULL,
    nonce BLOB NOT NULL,
    key_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (provider, project_id),
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    branch TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    worker_id TEXT,
    metadata TEXT,
    title TEXT,
    summary TEXT,
    prompt TEXT,
    response TEXT,
    paused INTEGER NOT NULL DEFAULT 0,
    compacted INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_sessions_project_id ON sessions(project_id);
CREATE INDEX idx_sessions_task_id ON sessions(task_id);
CREATE INDEX idx_sessions_status ON sessions(status);

CREATE TABLE task_memory_refs (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    relation TEXT NOT NULL DEFAULT 'context',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (task_id, note_id)
);

CREATE INDEX idx_task_memory_refs_note_id ON task_memory_refs(note_id);

CREATE TABLE epic_memory_refs (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    relation TEXT NOT NULL DEFAULT 'context',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (task_id, note_id)
);

CREATE INDEX idx_epic_memory_refs_note_id ON epic_memory_refs(note_id);

CREATE TABLE session_messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_session_messages_session_id_created_at
    ON session_messages(session_id, created_at);

CREATE TABLE verification_cache (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    command TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    file_count INTEGER NOT NULL,
    metadata TEXT,
    stdout TEXT,
    stderr TEXT,
    exit_code INTEGER NOT NULL,
    success INTEGER NOT NULL,
    duration_ms INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at TEXT NOT NULL,
    UNIQUE(project_id, command, fingerprint)
);

CREATE INDEX idx_verification_cache_project_command
    ON verification_cache(project_id, command);
CREATE INDEX idx_verification_cache_expires_at
    ON verification_cache(expires_at);

-- Note associations for Hebbian co-access learning (ADR-023)
CREATE TABLE note_associations (
    note_a_id       TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    note_b_id       TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    weight          REAL NOT NULL DEFAULT 0.01,
    co_access_count INTEGER NOT NULL DEFAULT 1,
    last_co_access  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (note_a_id, note_b_id),
    CHECK (note_a_id < note_b_id)
);

CREATE INDEX idx_note_associations_a ON note_associations(note_a_id);
CREATE INDEX idx_note_associations_b ON note_associations(note_b_id);
CREATE INDEX idx_note_associations_weight ON note_associations(weight);

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
