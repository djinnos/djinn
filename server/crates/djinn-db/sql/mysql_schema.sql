-- ADR-055 Dolt/MySQL schema snapshot for note/task/session state.
--
-- This snapshot intentionally covers the relational tables needed by the
-- staged MySQL/Dolt backend cutover while preserving the existing SQLite
-- runtime path. It removes SQLite-only FTS5 shadow tables, trigger-based sync,
-- and sqlite-vec virtual tables in favor of native MySQL/Dolt structures.

CREATE TABLE projects (
    id              VARCHAR(36) NOT NULL PRIMARY KEY,
    name            VARCHAR(255) NOT NULL,
    path            TEXT NOT NULL,
    created_at      DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    target_branch   VARCHAR(255) NOT NULL DEFAULT 'main',
    auto_merge      BOOLEAN NOT NULL DEFAULT FALSE,
    sync_enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    sync_remote     TEXT NULL,
    UNIQUE KEY uq_projects_name (name),
    UNIQUE KEY uq_projects_path (path(191))
);

CREATE TABLE tasks (
    id                         VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id                 VARCHAR(36) NOT NULL,
    title                      TEXT NOT NULL,
    description                LONGTEXT NOT NULL,
    status                     VARCHAR(64) NOT NULL DEFAULT 'todo',
    issue_type                 VARCHAR(64) NOT NULL DEFAULT 'task',
    parent_task_id             VARCHAR(36) NULL,
    priority                   INT NULL,
    labels                     LONGTEXT NOT NULL,
    assignee                   VARCHAR(255) NULL,
    batch_id                   VARCHAR(36) NULL,
    source_branch              VARCHAR(255) NULL,
    merge_commit_sha           VARCHAR(64) NULL,
    archived_at                DATETIME(3) NULL,
    blocked_reason             LONGTEXT NULL,
    blocked_at                 DATETIME(3) NULL,
    pm_override                BOOLEAN NOT NULL DEFAULT FALSE,
    pm_override_reason         LONGTEXT NULL,
    verification_failure_count INT NOT NULL DEFAULT 0,
    created_at                 DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    updated_at                 DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    CONSTRAINT fk_tasks_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_tasks_parent FOREIGN KEY (parent_task_id) REFERENCES tasks(id) ON DELETE SET NULL
);

CREATE INDEX idx_tasks_project_id ON tasks(project_id);
CREATE INDEX idx_tasks_project_status ON tasks(project_id, status);
CREATE INDEX idx_tasks_project_priority ON tasks(project_id, priority);
CREATE INDEX idx_tasks_project_parent ON tasks(project_id, parent_task_id);
CREATE INDEX idx_tasks_project_issue_type ON tasks(project_id, issue_type);
CREATE INDEX idx_tasks_project_archived ON tasks(project_id, archived_at);

CREATE TABLE task_blockers (
    task_id          VARCHAR(36) NOT NULL,
    blocker_task_id  VARCHAR(36) NOT NULL,
    created_at       DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (task_id, blocker_task_id),
    CONSTRAINT fk_task_blockers_task FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT fk_task_blockers_blocker FOREIGN KEY (blocker_task_id) REFERENCES tasks(id) ON DELETE CASCADE
);

CREATE INDEX idx_task_blockers_blocker ON task_blockers(blocker_task_id);

CREATE TABLE task_activity_log (
    id          VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id  VARCHAR(36) NOT NULL,
    task_id      VARCHAR(36) NOT NULL,
    actor_role  VARCHAR(64) NOT NULL,
    actor_id    VARCHAR(255) NULL,
    event_type  VARCHAR(128) NOT NULL,
    payload     LONGTEXT NOT NULL,
    created_at  DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    archived_at DATETIME(3) NULL,
    CONSTRAINT fk_task_activity_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_task_activity_task FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);

CREATE INDEX idx_task_activity_project_task_created
    ON task_activity_log(project_id, task_id, created_at DESC);
CREATE INDEX idx_task_activity_project_created
    ON task_activity_log(project_id, created_at DESC);
CREATE INDEX idx_task_activity_archived ON task_activity_log(archived_at);

CREATE TABLE notes (
    id            VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id     VARCHAR(36) NOT NULL,
    permalink     VARCHAR(255) NOT NULL,
    title         TEXT NOT NULL,
    file_path     TEXT NOT NULL,
    storage       VARCHAR(32) NOT NULL DEFAULT 'file',
    note_type     VARCHAR(64) NOT NULL,
    folder        VARCHAR(255) NOT NULL,
    tags          TEXT NOT NULL,
    content       LONGTEXT NOT NULL,
    created_at    DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    updated_at    DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    last_accessed DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    access_count  BIGINT NOT NULL DEFAULT 0,
    confidence    DOUBLE NOT NULL DEFAULT 1.0,
    abstract      TEXT NULL,
    overview      TEXT NULL,
    scope_paths   LONGTEXT NULL,
    content_hash  CHAR(64) NULL,
    CONSTRAINT fk_notes_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    UNIQUE KEY uq_notes_project_permalink (project_id, permalink),
    UNIQUE KEY uq_notes_project_file_path (project_id, file_path(191))
);

CREATE INDEX idx_notes_project_folder ON notes(project_id, folder);
CREATE INDEX idx_notes_project_updated ON notes(project_id, updated_at DESC);
CREATE INDEX idx_notes_project_last_accessed ON notes(project_id, last_accessed DESC);
CREATE INDEX idx_notes_project_content_hash ON notes(project_id, content_hash);
CREATE INDEX idx_notes_project_folder_title ON notes(project_id, folder, title(191));

-- MySQL/Dolt replacement for SQLite FTS5 shadow table + triggers.
ALTER TABLE notes ADD FULLTEXT KEY notes_ft (title, content, tags);

CREATE TABLE note_embeddings (
    note_id        VARCHAR(36) NOT NULL PRIMARY KEY,
    embedding      LONGBLOB NOT NULL,
    embedding_dim  INT NOT NULL,
    updated_at     DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    CONSTRAINT fk_note_embeddings_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE TABLE note_embedding_meta (
    note_id         VARCHAR(36) NOT NULL PRIMARY KEY,
    content_hash    CHAR(64) NOT NULL,
    embedded_at     DATETIME(3) NOT NULL,
    model_version   VARCHAR(255) NOT NULL,
    embedding_dim   INT NOT NULL,
    extension_state VARCHAR(64) NOT NULL DEFAULT 'pending',
    CONSTRAINT fk_note_embedding_meta_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX idx_note_embedding_meta_model_version
    ON note_embedding_meta(model_version);
CREATE INDEX idx_note_embedding_meta_embedded_at
    ON note_embedding_meta(embedded_at DESC);

CREATE TABLE note_links (
    source_note_id       VARCHAR(36) NOT NULL,
    target_permalink_raw VARCHAR(255) NOT NULL,
    target_note_id       VARCHAR(36) NULL,
    raw_text             TEXT NOT NULL,
    occurrence_index     INT NOT NULL,
    created_at           DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (source_note_id, occurrence_index),
    CONSTRAINT fk_note_links_source FOREIGN KEY (source_note_id) REFERENCES notes(id) ON DELETE CASCADE,
    CONSTRAINT fk_note_links_target FOREIGN KEY (target_note_id) REFERENCES notes(id) ON DELETE SET NULL
);

CREATE INDEX idx_note_links_source ON note_links(source_note_id);
CREATE INDEX idx_note_links_target ON note_links(target_note_id);
CREATE INDEX idx_note_links_target_raw ON note_links(target_permalink_raw);

CREATE TABLE sessions (
    id         VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id VARCHAR(36) NOT NULL,
    task_id    VARCHAR(36) NULL,
    branch     VARCHAR(255) NOT NULL,
    status     VARCHAR(64) NOT NULL,
    started_at DATETIME(3) NOT NULL,
    ended_at   DATETIME(3) NULL,
    worker_id  VARCHAR(255) NULL,
    metadata   LONGTEXT NULL,
    title      TEXT NULL,
    summary    LONGTEXT NULL,
    prompt     LONGTEXT NULL,
    response   LONGTEXT NULL,
    paused     BOOLEAN NOT NULL DEFAULT FALSE,
    compacted  BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT fk_sessions_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_sessions_task FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE SET NULL
);

CREATE INDEX idx_sessions_project_id ON sessions(project_id);
CREATE INDEX idx_sessions_task_id ON sessions(task_id);
CREATE INDEX idx_sessions_status ON sessions(status);

CREATE TABLE task_memory_refs (
    task_id     VARCHAR(36) NOT NULL,
    note_id     VARCHAR(36) NOT NULL,
    relation    VARCHAR(64) NOT NULL DEFAULT 'context',
    created_at  DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (task_id, note_id),
    CONSTRAINT fk_task_memory_refs_task FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT fk_task_memory_refs_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX idx_task_memory_refs_note_id ON task_memory_refs(note_id);

CREATE TABLE epic_memory_refs (
    task_id     VARCHAR(36) NOT NULL,
    note_id     VARCHAR(36) NOT NULL,
    relation    VARCHAR(64) NOT NULL DEFAULT 'context',
    created_at  DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (task_id, note_id),
    CONSTRAINT fk_epic_memory_refs_task FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT fk_epic_memory_refs_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX idx_epic_memory_refs_note_id ON epic_memory_refs(note_id);

CREATE TABLE session_messages (
    id         VARCHAR(36) NOT NULL PRIMARY KEY,
    session_id VARCHAR(36) NOT NULL,
    role       VARCHAR(64) NOT NULL,
    content    LONGTEXT NOT NULL,
    created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    CONSTRAINT fk_session_messages_session FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX idx_session_messages_session_id_created_at
    ON session_messages(session_id, created_at);

CREATE TABLE note_associations (
    note_a_id       VARCHAR(36) NOT NULL,
    note_b_id       VARCHAR(36) NOT NULL,
    weight          DOUBLE NOT NULL DEFAULT 0.01,
    co_access_count BIGINT NOT NULL DEFAULT 1,
    last_co_access  DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (note_a_id, note_b_id),
    CONSTRAINT fk_note_associations_a FOREIGN KEY (note_a_id) REFERENCES notes(id) ON DELETE CASCADE,
    CONSTRAINT fk_note_associations_b FOREIGN KEY (note_b_id) REFERENCES notes(id) ON DELETE CASCADE,
    CONSTRAINT chk_note_association_order CHECK (note_a_id < note_b_id)
);

CREATE INDEX idx_note_associations_a ON note_associations(note_a_id);
CREATE INDEX idx_note_associations_b ON note_associations(note_b_id);
CREATE INDEX idx_note_associations_weight ON note_associations(weight);

CREATE TABLE consolidated_note_provenance (
    note_id    VARCHAR(36) NOT NULL,
    session_id VARCHAR(36) NOT NULL,
    created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (note_id, session_id),
    CONSTRAINT fk_consolidated_note_provenance_note FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE,
    CONSTRAINT fk_consolidated_note_provenance_session FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX idx_consolidated_note_provenance_session_id
    ON consolidated_note_provenance(session_id);

CREATE TABLE consolidation_run_metrics (
    id                         VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id                 VARCHAR(36) NOT NULL,
    note_type                  VARCHAR(64) NOT NULL,
    status                     VARCHAR(64) NOT NULL,
    scanned_note_count         INT NOT NULL,
    candidate_cluster_count    INT NOT NULL,
    consolidated_cluster_count INT NOT NULL,
    consolidated_note_count    INT NOT NULL,
    source_note_count          INT NOT NULL,
    started_at                 DATETIME(3) NOT NULL,
    completed_at               DATETIME(3) NULL,
    error_message              LONGTEXT NULL,
    CONSTRAINT fk_consolidation_metrics_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX idx_consolidation_run_metrics_project_note_type_started_at
    ON consolidation_run_metrics(project_id, note_type, started_at DESC);
