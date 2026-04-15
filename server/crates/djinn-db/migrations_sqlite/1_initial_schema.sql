-- SQLite initial schema (flattened from refinery migrations V20260302..V20260415)
-- This file is the single source of truth for fresh SQLite installs.
-- Managed by sqlx::migrate!. DO NOT MODIFY after commit — add a new V{N}__{slug}.sql instead.


-- ── V20260302000001__initial_schema.sql ──
-- Initial schema: settings and projects tables.

CREATE TABLE settings (
    key        TEXT NOT NULL PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE projects (
    id         TEXT NOT NULL PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    path       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- ── V20260303000001__task_board.sql ──
-- Task board schema: epics, tasks, blockers, activity_log.
--
-- Primary keys are UUIDv7 (RFC 9562) stored as TEXT in canonical lowercase hex.
-- Short IDs are 4-char base36 strings (0-9a-z), unique per table, generated at
-- insert time with retry-on-collision logic in the repository layer.

CREATE TABLE epics (
    id          TEXT NOT NULL PRIMARY KEY,
    short_id    TEXT NOT NULL UNIQUE,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    emoji       TEXT NOT NULL DEFAULT '',
    color       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'open'
                     CHECK(status IN ('open', 'closed')),
    owner       TEXT NOT NULL DEFAULT '',
    memory_refs TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at   TEXT
);

CREATE TABLE tasks (
    id                  TEXT NOT NULL PRIMARY KEY,
    short_id            TEXT NOT NULL UNIQUE,
    epic_id             TEXT NOT NULL REFERENCES epics(id) ON DELETE CASCADE,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task'
                             CHECK(issue_type IN ('feature', 'task', 'bug')),
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'draft', 'backlog', 'open', 'in_progress',
                                 'needs_task_review', 'in_task_review',
                                 'needs_epic_review', 'in_epic_review',
                                 'approved', 'closed', 'blocked'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT
);

-- Indexes for common query patterns.
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status   ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

-- Blocker relationships: task_id is blocked by blocking_task_id.
CREATE TABLE blockers (
    task_id          TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    blocking_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, blocking_task_id)
);

-- Append-only audit trail for all task mutations and comments.
-- task_id has no FK so log entries survive task deletion.
CREATE TABLE activity_log (
    id          TEXT NOT NULL PRIMARY KEY,
    task_id     TEXT,
    actor_id    TEXT NOT NULL DEFAULT '',
    actor_role  TEXT NOT NULL DEFAULT '',
    event_type  TEXT NOT NULL,
    payload     TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX activity_log_task_id ON activity_log(task_id);

-- ── V20260303000002__notes.sql ──
-- Knowledge base notes schema.
--
-- Notes are markdown files on disk; this table is the search index.
-- Notes are scoped to a project (project_id FK to projects).
-- FTS5 virtual table provides BM25-ranked full-text search.

CREATE TABLE notes (
    id            TEXT NOT NULL PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    permalink     TEXT NOT NULL,    -- slug path e.g. "decisions/my-adr"
    title         TEXT NOT NULL,
    file_path     TEXT NOT NULL,    -- absolute path to .md file on disk
    note_type     TEXT NOT NULL DEFAULT '',
    folder        TEXT NOT NULL DEFAULT '',
    tags          TEXT NOT NULL DEFAULT '[]', -- JSON array
    content       TEXT NOT NULL DEFAULT '',   -- markdown body (no frontmatter)
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_accessed TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (project_id, permalink)
);

CREATE INDEX notes_project_id ON notes(project_id);
CREATE INDEX notes_folder     ON notes(folder);
CREATE INDEX notes_type       ON notes(note_type);
CREATE INDEX notes_updated_at ON notes(updated_at);

-- FTS5 external content table for BM25-ranked full-text search.
-- content='notes' tells FTS5 to retrieve content from the notes table.
-- content_rowid='rowid' maps FTS rowids to notes implicit rowids.
CREATE VIRTUAL TABLE notes_fts USING fts5(
    title,
    content,
    tags,
    content='notes',
    content_rowid='rowid',
    tokenize='unicode61'
);

-- Triggers keep the FTS5 index in sync with notes rows.
CREATE TRIGGER notes_fts_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER notes_fts_au AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
    INSERT INTO notes_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER notes_fts_ad AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
END;

-- ── V20260303000003__task_state_fields.sql ──
-- Add state-tracking columns to tasks for full state machine support.
ALTER TABLE tasks ADD COLUMN blocked_from_status TEXT;
ALTER TABLE tasks ADD COLUMN close_reason TEXT;

-- ── V20260303000006__model_health.sql ──
-- Custom provider registry for user-added OpenAI-compatible providers.
--
-- Each row is an unlisted provider the user has registered via provider_add_custom.
-- seed_models is a JSON array of {id, name} objects to pre-populate the model picker.

CREATE TABLE custom_providers (
    id          TEXT NOT NULL PRIMARY KEY,
    name        TEXT NOT NULL,
    base_url    TEXT NOT NULL,
    env_var     TEXT NOT NULL,
    seed_models TEXT NOT NULL DEFAULT '[]',  -- JSON: [{id, name}, ...]
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- ── V20260303000007__note_links.sql ──
-- Wikilink edges between knowledge base notes.
--
-- Each [[Target]] in note content becomes one row here.
-- source_id → target note that contains the link.
-- target_id → resolved note (NULL = broken link, target doesn't exist).
-- Rows cascade-delete when the source note is deleted.
-- target_id is SET NULL when the target note is deleted.

CREATE TABLE note_links (
    id           TEXT NOT NULL PRIMARY KEY,
    source_id    TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    target_id    TEXT REFERENCES notes(id) ON DELETE SET NULL,
    target_raw   TEXT NOT NULL,   -- raw text inside [[...]] (before pipe)
    display_text TEXT,            -- alias text after | if present
    UNIQUE (source_id, target_raw)
);

CREATE INDEX note_links_source ON note_links(source_id);
CREATE INDEX note_links_target ON note_links(target_id);

-- ── V20260303000008__task_memory_refs.sql ──
-- Add memory_refs column to tasks for bidirectional note-task linking.
ALTER TABLE tasks ADD COLUMN memory_refs TEXT NOT NULL DEFAULT '[]';

-- ── V20260303000009__credentials.sql ──
-- Credential vault: encrypted API key storage for Goose provider dispatch.
--
-- key_name is UNIQUE — one stored value per env-var name (e.g. ANTHROPIC_API_KEY).
-- encrypted_value stores: nonce (12 bytes) || AES-256-GCM ciphertext+tag.
-- The encryption key is derived from machine identity (hostname + user).

CREATE TABLE credentials (
    id              TEXT NOT NULL PRIMARY KEY,
    provider_id     TEXT NOT NULL,
    key_name        TEXT NOT NULL UNIQUE,
    encrypted_value BLOB NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX credentials_provider_id ON credentials(provider_id);

-- ── V20260303000010__sessions.sql ──
-- Persist agent session lifecycle + token accounting per task.

CREATE TABLE sessions (
    id            TEXT NOT NULL PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    model_id      TEXT NOT NULL,
    agent_type    TEXT NOT NULL,
    started_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at      TEXT,
    status        TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed')),
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    worktree_path TEXT
);

CREATE INDEX sessions_task_id ON sessions(task_id);
CREATE INDEX sessions_status ON sessions(status);

-- ── V20260303000011__task_merge_commit_sha.sql ──
-- Add merge_commit_sha to tasks for per-task squash-merge traceability.
ALTER TABLE tasks ADD COLUMN merge_commit_sha TEXT;

-- ── V20260304000001__project_scoping.sql ──
-- Scope epics/tasks/sessions by project_id.
-- Also rename phase-review statuses to epic-review statuses.

PRAGMA foreign_keys = OFF;

-- Backfill strategy:
-- - If projects already exist, attach legacy rows to the first project.
-- - If no project exists and there is pre-scoped data, create one migration
--   project so project_id FKs can be satisfied.
DROP TABLE IF EXISTS _migration_project;
CREATE TEMP TABLE _migration_project (
    id TEXT NOT NULL
);

INSERT INTO projects (id, name, path)
SELECT lower(hex(randomblob(16))), 'legacy-migrated', 'legacy://migration'
WHERE (SELECT COUNT(*) FROM projects) = 0
  AND (
      (SELECT COUNT(*) FROM epics)
    + (SELECT COUNT(*) FROM tasks)
    + (SELECT COUNT(*) FROM sessions)
  ) > 0;

INSERT INTO _migration_project(id)
SELECT id FROM projects ORDER BY created_at LIMIT 1;

-- EPICS -----------------------------------------------------------------------
DROP TABLE IF EXISTS epics_new;
CREATE TABLE epics_new (
    id          TEXT NOT NULL PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id    TEXT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    emoji       TEXT NOT NULL DEFAULT '',
    color       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'open'
                     CHECK(status IN ('open', 'closed')),
    owner       TEXT NOT NULL DEFAULT '',
    memory_refs TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at   TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO epics_new (
    id, project_id, short_id, title, description, emoji, color, status,
    owner, memory_refs, created_at, updated_at, closed_at
)
SELECT
    e.id,
    (SELECT id FROM _migration_project LIMIT 1),
    e.short_id,
    e.title,
    e.description,
    e.emoji,
    e.color,
    e.status,
    e.owner,
    COALESCE(e.memory_refs, '[]'),
    e.created_at,
    e.updated_at,
    e.closed_at
FROM epics e;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

-- TASKS -----------------------------------------------------------------------
DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task'
                             CHECK(issue_type IN ('feature', 'task', 'bug')),
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'draft', 'backlog', 'open', 'in_progress',
                                 'needs_task_review', 'in_task_review',
                                 'needs_epic_review', 'in_epic_review',
                                 'approved', 'closed', 'blocked'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    blocked_from_status TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design,
    issue_type, status, priority, owner, labels, acceptance_criteria,
    reopen_count, continuation_count, created_at, updated_at, closed_at,
    blocked_from_status, close_reason, merge_commit_sha, memory_refs
)
SELECT
    t.id,
    (SELECT id FROM _migration_project LIMIT 1),
    t.short_id,
    t.epic_id,
    t.title,
    t.description,
    t.design,
    t.issue_type,
    t.status,
    t.priority,
    t.owner,
    t.labels,
    t.acceptance_criteria,
    t.reopen_count,
    t.continuation_count,
    t.created_at,
    t.updated_at,
    t.closed_at,
    t.blocked_from_status,
    t.close_reason,
    t.merge_commit_sha,
    COALESCE(t.memory_refs, '[]')
FROM tasks t;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

-- SESSIONS --------------------------------------------------------------------
DROP TABLE IF EXISTS sessions_new;
CREATE TABLE sessions_new (
    id            TEXT NOT NULL PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id       TEXT REFERENCES tasks(id) ON DELETE CASCADE,
    model_id      TEXT NOT NULL,
    agent_type    TEXT NOT NULL,
    started_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at      TEXT,
    status        TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed')),
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    worktree_path TEXT
);

INSERT INTO sessions_new (
    id, project_id, task_id, model_id, agent_type,
    started_at, ended_at, status, tokens_in, tokens_out, worktree_path
)
SELECT
    s.id,
    COALESCE(t.project_id, (SELECT id FROM _migration_project LIMIT 1)),
    s.task_id,
    s.model_id,
    s.agent_type,
    s.started_at,
    s.ended_at,
    s.status,
    s.tokens_in,
    s.tokens_out,
    s.worktree_path
FROM sessions s
LEFT JOIN tasks t ON t.id = s.task_id;

DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX sessions_project_id ON sessions(project_id);
CREATE INDEX sessions_task_id ON sessions(task_id);
CREATE INDEX sessions_status ON sessions(status);
CREATE INDEX sessions_project_agent_status ON sessions(project_id, agent_type, status);

DROP TABLE _migration_project;

PRAGMA foreign_keys = ON;

-- ── V20260304130000__epic_review_batches.sql ──
-- Add epic in_review lifecycle status and explicit epic review batch tables.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS epics_new;
CREATE TABLE epics_new (
    id          TEXT NOT NULL PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id    TEXT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    emoji       TEXT NOT NULL DEFAULT '',
    color       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'open'
                     CHECK(status IN ('open', 'in_review', 'closed')),
    owner       TEXT NOT NULL DEFAULT '',
    memory_refs TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at   TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO epics_new (
    id, project_id, short_id, title, description, emoji, color, status,
    owner, memory_refs, created_at, updated_at, closed_at
)
SELECT
    id, project_id, short_id, title, description, emoji, color, status,
    owner, COALESCE(memory_refs, '[]'), created_at, updated_at, closed_at
FROM epics;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

CREATE TABLE epic_review_batches (
    id             TEXT NOT NULL PRIMARY KEY,
    project_id     TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    epic_id        TEXT NOT NULL REFERENCES epics(id) ON DELETE CASCADE,
    status         TEXT NOT NULL DEFAULT 'queued'
                       CHECK(status IN ('queued', 'in_review', 'clean', 'issues_found', 'cancelled')),
    verdict_reason TEXT,
    session_id     TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at     TEXT,
    completed_at   TEXT
);

CREATE INDEX epic_review_batches_project_id ON epic_review_batches(project_id);
CREATE INDEX epic_review_batches_epic_id ON epic_review_batches(epic_id);
CREATE INDEX epic_review_batches_status ON epic_review_batches(status);

CREATE TABLE epic_review_batch_tasks (
    batch_id    TEXT NOT NULL REFERENCES epic_review_batches(id) ON DELETE CASCADE,
    task_id     TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (batch_id, task_id)
);

CREATE INDEX epic_review_batch_tasks_task_id ON epic_review_batch_tasks(task_id);

PRAGMA foreign_keys = ON;

-- ── V20260304170000__remove_legacy_epic_review_statuses.sql ──
-- Remove legacy per-task epic review statuses now that epic review is batch-driven.
-- Any lingering legacy review statuses are normalized to closed.

PRAGMA foreign_keys = OFF;

UPDATE tasks
SET
    status = 'closed',
    closed_at = COALESCE(closed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    close_reason = COALESCE(close_reason, 'completed'),
    blocked_from_status = CASE
        WHEN blocked_from_status IN ('needs_epic_review', 'in_epic_review', 'approved')
            THEN 'open'
        ELSE blocked_from_status
    END,
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE status IN ('needs_epic_review', 'in_epic_review', 'approved')
   OR blocked_from_status IN ('needs_epic_review', 'in_epic_review', 'approved');

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task'
                             CHECK(issue_type IN ('feature', 'task', 'bug')),
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'draft', 'backlog', 'open', 'in_progress',
                                 'needs_task_review', 'in_task_review',
                                 'closed', 'blocked'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    blocked_from_status TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, created_at, updated_at, closed_at, blocked_from_status,
    close_reason, merge_commit_sha, memory_refs
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, created_at, updated_at, closed_at, blocked_from_status,
    close_reason, merge_commit_sha, memory_refs
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260305000001__add_project_setup_verification_commands.sql ──
-- Add setup_commands and verification_commands JSON arrays to projects.
ALTER TABLE projects ADD COLUMN setup_commands        TEXT NOT NULL DEFAULT '[]';
ALTER TABLE projects ADD COLUMN verification_commands TEXT NOT NULL DEFAULT '[]';

-- ── V20260305000002__add_session_goose_id_and_paused.sql ──
-- Add goose_session_id column and 'paused' status variant to sessions.
--
-- goose_session_id links Djinn's session record to Goose's internal session
-- storage (~/.djinn/sessions/sessions.db) for resume capability (ADR-015).
-- 'paused' status records sessions that were interrupted and may be resumed.
--
-- SQLite does not support ALTER TABLE ... MODIFY CONSTRAINT, so we recreate
-- the table to update the CHECK constraint.

DROP TABLE IF EXISTS sessions_new;
CREATE TABLE sessions_new (
    id               TEXT NOT NULL PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id          TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    model_id         TEXT NOT NULL,
    agent_type       TEXT NOT NULL,
    started_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at         TEXT,
    status           TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed', 'paused')),
    tokens_in        INTEGER NOT NULL DEFAULT 0,
    tokens_out       INTEGER NOT NULL DEFAULT 0,
    worktree_path    TEXT,
    goose_session_id TEXT
);

INSERT INTO sessions_new
    SELECT id, project_id, task_id, model_id, agent_type,
           started_at, ended_at, status, tokens_in, tokens_out,
           worktree_path, NULL
    FROM sessions;

DROP TABLE sessions;

ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX sessions_project_id ON sessions(project_id);
CREATE INDEX sessions_task_id    ON sessions(task_id);
CREATE INDEX sessions_status     ON sessions(status);

-- ── V20260305000003__add_session_continuation_of.sql ──
-- Add continuation_of column to sessions table.
--
-- Links a compaction-triggered continuation session to its predecessor,
-- forming a chain that the UI can group into a single logical session timeline.
-- Reference: ADR-018 (Session Continuity Option C).

ALTER TABLE sessions ADD COLUMN continuation_of TEXT REFERENCES sessions(id);

CREATE INDEX idx_sessions_continuation_of ON sessions(continuation_of);

-- ── V20260305000004__add_session_compacted_status.sql ──
-- Add 'compacted' status variant to sessions CHECK constraint.
--
-- The compaction feature (ADR-018) marks old sessions as 'compacted' when a
-- summary is generated and the agent continues in a new continuation session.
-- The CHECK constraint was missing this value, causing DB errors.
--
-- SQLite does not support ALTER TABLE ... MODIFY CONSTRAINT, so we recreate
-- the table to update the CHECK constraint.

DROP TABLE IF EXISTS sessions_new;
CREATE TABLE sessions_new (
    id               TEXT NOT NULL PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id          TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    model_id         TEXT NOT NULL,
    agent_type       TEXT NOT NULL,
    started_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at         TEXT,
    status           TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed', 'paused', 'compacted')),
    tokens_in        INTEGER NOT NULL DEFAULT 0,
    tokens_out       INTEGER NOT NULL DEFAULT 0,
    worktree_path    TEXT,
    goose_session_id TEXT,
    continuation_of  TEXT REFERENCES sessions_new(id)
);

INSERT INTO sessions_new
    SELECT id, project_id, task_id, model_id, agent_type,
           started_at, ended_at, status, tokens_in, tokens_out,
           worktree_path, goose_session_id, continuation_of
    FROM sessions;

DROP TABLE sessions;

ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX sessions_project_id ON sessions(project_id);
CREATE INDEX sessions_task_id    ON sessions(task_id);
CREATE INDEX sessions_status     ON sessions(status);
CREATE INDEX idx_sessions_continuation_of ON sessions(continuation_of);

-- ── V20260305120000__remove_blocked_status.sql ──
-- Remove the blocked status concept. Tasks no longer enter a blocked state;
-- dependency ordering is handled entirely by the blocker relationship table.

ALTER TABLE tasks DROP COLUMN blocked_from_status;

-- SQLite does not support dropping CHECK constraints inline.
-- The status column check is enforced at the application layer (TaskStatus::parse).

-- ── V20260305121500__add_project_config_fields.sql ──
ALTER TABLE projects ADD COLUMN target_branch TEXT NOT NULL DEFAULT 'main';
ALTER TABLE projects ADD COLUMN auto_merge INTEGER NOT NULL DEFAULT 1;
ALTER TABLE projects ADD COLUMN sync_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN sync_remote TEXT;

-- ── V20260308000001__drop_session_continuation_of.sql ──
DROP INDEX IF EXISTS idx_sessions_continuation_of;
ALTER TABLE sessions DROP COLUMN continuation_of;

-- ── V20260308000002__add_verifying_status.sql ──
-- Add 'verifying' task status for background verification after agent session.
-- This was originally a no-op — the actual table rebuild is in V20260309000002.

-- ── V20260309000001__drop_epic_review_batches.sql ──
-- Remove epic review batch tables — epic batch review system has been removed.
DROP TABLE IF EXISTS epic_review_batch_tasks;
DROP TABLE IF EXISTS epic_review_batches;

-- ── V20260309000002__rebuild_tasks_add_verifying.sql ──
-- Rebuild tasks table to update CHECK constraint — add 'verifying' status
-- and remove stale 'blocked' status.
-- SQLite does not support ALTER CHECK, so table must be recreated.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task'
                             CHECK(issue_type IN ('feature', 'task', 'bug')),
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'draft', 'backlog', 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, created_at, updated_at, closed_at,
    close_reason, merge_commit_sha, memory_refs
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, created_at, updated_at, closed_at,
    close_reason, merge_commit_sha, memory_refs
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260309000003__rebuild_tasks_pm_intervention.sql ──
-- Rebuild tasks table to update CHECK constraint — add 'needs_pm_intervention'
-- and 'in_pm_intervention' statuses for the PM intervention circuit breaker.
-- SQLite does not support ALTER CHECK, so table must be recreated.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task'
                             CHECK(issue_type IN ('feature', 'task', 'bug')),
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'draft', 'backlog', 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'needs_pm_intervention', 'in_pm_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, created_at, updated_at, closed_at,
    close_reason, merge_commit_sha, memory_refs
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, created_at, updated_at, closed_at,
    close_reason, merge_commit_sha, memory_refs
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260309000004__activity_log_archived.sql ──
-- Add soft-delete support to activity_log.
-- SQLite supports ADD COLUMN for non-NOT-NULL columns.
ALTER TABLE activity_log ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;

-- ── V20260309000005__session_messages.sql ──
-- Conversation message storage for agent sessions.
-- Replaces Goose's separate sessions.db per ADR-027.

CREATE TABLE session_messages (
    id           TEXT NOT NULL PRIMARY KEY,
    session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role         TEXT NOT NULL CHECK(role IN ('system', 'user', 'assistant')),
    content_json TEXT NOT NULL,
    token_count  INTEGER,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX session_messages_session_id ON session_messages(session_id);

-- ── V20260310000001__verification_failure_count.sql ──
-- Add a separate counter for consecutive verification failures.
-- Resets to 0 on VerificationPass; incremented on VerificationFail.
-- After 3 consecutive failures the task escalates to PM intervention.
ALTER TABLE tasks ADD COLUMN verification_failure_count INTEGER NOT NULL DEFAULT 0;

-- ── V20260312000001__rebuild_tasks_add_backlog_status.sql ──
-- Rebuild tasks table to add missing 'backlog' status to CHECK constraint.
-- The status was present in the Rust enum but was missing from the DB constraint
-- due to a post-apply edit of migration V20260309000003.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task'
                             CHECK(issue_type IN ('feature', 'task', 'bug')),
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'draft', 'backlog', 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'needs_pm_intervention', 'in_pm_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    CASE WHEN status = 'draft' THEN 'backlog' ELSE status END,
    priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260312000002__sessions_nullable_task_id.sql ──
-- Make sessions.task_id nullable so project-scoped agents (e.g. groomer)
-- can create sessions without a real task row in the tasks table.

DROP TABLE IF EXISTS sessions_new;
CREATE TABLE sessions_new (
    id               TEXT NOT NULL PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id          TEXT REFERENCES tasks(id) ON DELETE CASCADE,
    model_id         TEXT NOT NULL,
    agent_type       TEXT NOT NULL,
    started_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at         TEXT,
    status           TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed', 'paused', 'compacted')),
    tokens_in        INTEGER NOT NULL DEFAULT 0,
    tokens_out       INTEGER NOT NULL DEFAULT 0,
    worktree_path    TEXT,
    goose_session_id TEXT
);

INSERT INTO sessions_new
    SELECT id, project_id, task_id, model_id, agent_type,
           started_at, ended_at, status, tokens_in, tokens_out,
           worktree_path, goose_session_id
    FROM sessions;

DROP TABLE sessions;

ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX sessions_project_id ON sessions(project_id);
CREATE INDEX sessions_task_id    ON sessions(task_id);
CREATE INDEX sessions_status     ON sessions(status);

-- ── V20260313000001__epic_remove_in_review_status.sql ──
-- Simplify epic statuses: remove in_review, keep only open and closed.
-- Any existing in_review epics become open.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS epics_new;
CREATE TABLE epics_new (
    id          TEXT NOT NULL PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id    TEXT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    emoji       TEXT NOT NULL DEFAULT '',
    color       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'open'
                     CHECK(status IN ('open', 'closed')),
    owner       TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at   TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO epics_new (
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at
)
SELECT
    id, project_id, short_id, title, description, emoji, color,
    CASE WHEN status = 'in_review' THEN 'open' ELSE status END,
    owner, created_at, updated_at,
    CASE WHEN status = 'in_review' THEN NULL ELSE closed_at END
FROM epics;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

PRAGMA foreign_keys = ON;

-- ── V20260313000002__epic_memory_refs.sql ──
ALTER TABLE epics ADD COLUMN memory_refs TEXT NOT NULL DEFAULT '[]';

-- ── V20260314000001__add_verification_cache.sql ──
CREATE TABLE verification_cache (
    project_id  TEXT NOT NULL,
    commit_sha  TEXT NOT NULL,
    output      TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (project_id, commit_sha)
);

-- ── V20260314000002__drop_project_command_columns.sql ──
-- ADR-030: Commands now live in .djinn/settings.json, not in the DB.
-- SQLite doesn't support DROP COLUMN before 3.35.0; use table rebuild.
CREATE TABLE projects_new (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    path        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    target_branch TEXT NOT NULL DEFAULT 'main',
    auto_merge    INTEGER NOT NULL DEFAULT 0,
    sync_enabled  INTEGER NOT NULL DEFAULT 0,
    sync_remote   TEXT
);

INSERT INTO projects_new (id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote)
    SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote
    FROM projects;

DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

-- ── V20260317000001__notes_cognitive_columns.sql ──
ALTER TABLE notes ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE notes ADD COLUMN confidence REAL NOT NULL DEFAULT 1.0;
ALTER TABLE notes ADD COLUMN abstract TEXT;
ALTER TABLE notes ADD COLUMN overview TEXT;

-- ── V20260317000002__note_associations.sql ──
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

-- ── V20260318000001__drop_session_goose_id.sql ──
-- Drop the obsolete Goose-linked session column from sessions.

DROP TABLE IF EXISTS sessions_new;
CREATE TABLE sessions_new (
    id            TEXT NOT NULL PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id       TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    model_id      TEXT NOT NULL,
    agent_type    TEXT NOT NULL,
    started_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at      TEXT,
    status        TEXT NOT NULL CHECK(status IN ('running', 'completed', 'interrupted', 'failed', 'paused', 'compacted')),
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    worktree_path TEXT
);

INSERT INTO sessions_new
    SELECT id, project_id, task_id, model_id, agent_type,
           started_at, ended_at, status, tokens_in, tokens_out,
           worktree_path
    FROM sessions;

DROP TABLE sessions;

ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX sessions_project_id ON sessions(project_id);
CREATE INDEX sessions_task_id    ON sessions(task_id);
CREATE INDEX sessions_status     ON sessions(status);

-- ── V20260319000001__task_merge_conflict_metadata.sql ──
ALTER TABLE tasks ADD COLUMN merge_conflict_metadata TEXT;

-- ── V20260319000002__remove_backlog_status.sql ──
-- Remove 'backlog' status from task lifecycle.
-- Tasks are now created directly in 'open' state; convert any existing backlog tasks to open.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task'
                             CHECK(issue_type IN ('feature', 'task', 'bug')),
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'needs_pm_intervention', 'in_pm_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    merge_conflict_metadata TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    CASE WHEN status IN ('backlog', 'draft') THEN 'open' ELSE status END,
    priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260319000003__agent_roles.sql ──
-- Agent roles: configurable per-project role definitions (default + specialist instances).

CREATE TABLE IF NOT EXISTS agent_roles (
    id                       TEXT NOT NULL PRIMARY KEY,
    project_id               TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name                     TEXT NOT NULL,
    base_role                TEXT NOT NULL
                                  CHECK(base_role IN (
                                      'worker', 'lead', 'planner',
                                      'architect', 'reviewer', 'resolver'
                                  )),
    description              TEXT NOT NULL DEFAULT '',
    system_prompt_extensions TEXT NOT NULL DEFAULT '',
    model_preference         TEXT,
    verification_command     TEXT,
    mcp_servers              TEXT NOT NULL DEFAULT '[]',
    skills                   TEXT NOT NULL DEFAULT '[]',
    is_default               INTEGER NOT NULL DEFAULT 0,
    created_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(project_id, name)
);

CREATE INDEX agent_roles_project_id     ON agent_roles(project_id);
CREATE INDEX agent_roles_base_role      ON agent_roles(project_id, base_role);
CREATE INDEX agent_roles_is_default     ON agent_roles(project_id, is_default);

-- ── V20260319000004__agent_roles_learned_prompt.sql ──
-- Add learned_prompt to agent_roles for auto-improvement loop.
-- Kept separate from user-written system_prompt_extensions (never auto-modified).

ALTER TABLE agent_roles ADD COLUMN learned_prompt TEXT;

-- Audit trail: every keep/discard decision by the Architect improvement loop.
CREATE TABLE IF NOT EXISTS learned_prompt_history (
    id              TEXT NOT NULL PRIMARY KEY,
    role_id         TEXT NOT NULL REFERENCES agent_roles(id) ON DELETE CASCADE,
    proposed_text   TEXT NOT NULL,
    action          TEXT NOT NULL CHECK(action IN ('keep', 'discard')),
    metrics_before  TEXT,
    metrics_after   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX learned_prompt_history_role_id ON learned_prompt_history(role_id);

-- ── V20260319000005__agent_roles_default_constraint.sql ──
-- Enforce: at most one default role per base_role per project.
-- SQLite supports partial unique indexes, so this is fully DB-enforced.

CREATE UNIQUE INDEX agent_roles_one_default_per_base_role
    ON agent_roles(project_id, base_role)
    WHERE is_default = 1;

-- ── V20260319000006__session_event_taxonomy.sql ──
-- Add event_taxonomy column to sessions for structural session extraction.
-- Stores a JSON blob with: files_changed, errors, git_ops, tools_used,
-- notes_read, notes_written, tasks_transitioned.
ALTER TABLE sessions ADD COLUMN event_taxonomy TEXT;

-- ── V20260319000007__task_agent_type.sql ──
-- Add agent_type to tasks so the Planner can route tasks to specialist roles.
ALTER TABLE tasks ADD COLUMN agent_type TEXT;

-- ── V20260319000008__task_issue_type_extended.sql ──
-- Extend task issue_type to support spike, research, decomposition, and review.
-- SQLite TEXT has no enum constraint at the DB layer; validation is enforced by
-- the application (djinn-mcp validate_issue_type). This migration is a schema
-- documentation marker so the migration version is recorded in refinery's history.
-- No structural change is needed: the column already exists as free-form TEXT.
SELECT 1; -- no-op statement to satisfy refinery's migration runner

-- ── V20260319000009__add_project_verification_rules.sql ──
-- Add verification_rules JSON column to projects.
-- Each rule is a { match_pattern: string, commands: [string] } object.
-- Stored as a JSON array; defaults to empty (no rules = fall back to full-project verification).
ALTER TABLE projects ADD COLUMN verification_rules TEXT NOT NULL DEFAULT '[]';

-- ── V20260319000010__task_issue_type_drop_check.sql ──
-- Remove CHECK constraint on issue_type so the extended set of types
-- (spike, review, decomposition, research) can be stored.
-- Application-level validation is done in djinn-mcp (validate_issue_type).

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task',
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'needs_pm_intervention', 'in_pm_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    merge_conflict_metadata TEXT,
    agent_type          TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260320000001__remove_issue_type_check_constraint.sql ──
-- Remove the restrictive CHECK constraint on issue_type so that spike, research,
-- decomposition, and review tasks can be stored. Validation is enforced by the
-- application layer (djinn-mcp validate_issue_type / djinn-core IssueType::parse).
-- SQLite does not support ALTER TABLE DROP CONSTRAINT, so the table must be rebuilt.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task',
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'needs_pm_intervention', 'in_pm_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    merge_conflict_metadata TEXT,
    agent_type          TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260320000002__add_pr_ready_status.sql ──
-- ADR-pefb: Add pr_ready task status
-- PR created, waiting for CI/review/merge. Distinct from closed (merged to main).
-- Dependent tasks do NOT unblock on pr_ready; only closed unblocks dependents.
--
-- SQLite does not support ALTER TABLE ADD CHECK, so the table must be rebuilt.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task',
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'pr_ready',
                                 'needs_pm_intervention', 'in_pm_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    merge_conflict_metadata TEXT,
    agent_type          TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260320000003__add_task_pr_url.sql ──
-- ADR-a8le: Add pr_url column to tasks table.
-- Stores the GitHub PR URL created by the reviewer when GitHub App is connected.
-- NULL when the direct-push merge path is used (no GitHub App).
ALTER TABLE tasks ADD COLUMN pr_url TEXT;

-- ── V20260320000004__learned_prompt_history_confirmed_action.sql ──
-- Extend learned_prompt_history.action to allow 'confirmed' (post-evaluation keep).
-- SQLite does not support ALTER COLUMN, so we rebuild the table.
-- Previous values: 'keep' (proposed, pending evaluation), 'discard' (reverted).
-- New value: 'confirmed' (evaluated post N tasks, metrics improved or neutral).

CREATE TABLE learned_prompt_history_new (
    id              TEXT NOT NULL PRIMARY KEY,
    role_id         TEXT NOT NULL REFERENCES agent_roles(id) ON DELETE CASCADE,
    proposed_text   TEXT NOT NULL,
    action          TEXT NOT NULL CHECK(action IN ('keep', 'discard', 'confirmed')),
    metrics_before  TEXT,
    metrics_after   TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO learned_prompt_history_new
    SELECT id, role_id, proposed_text, action, metrics_before, metrics_after, created_at
    FROM learned_prompt_history;

DROP TABLE learned_prompt_history;

ALTER TABLE learned_prompt_history_new RENAME TO learned_prompt_history;

CREATE INDEX learned_prompt_history_role_id ON learned_prompt_history(role_id);

-- ── V20260320000005__rename_pm_to_lead_statuses.sql ──
-- ADR-034 §1: rename PM → Lead in task status values.
-- Rebuilds the tasks table to update the CHECK constraint, migrating
-- existing 'needs_pm_intervention' / 'in_pm_intervention' rows in the
-- same INSERT SELECT.
--
-- SQLite does not support ALTER TABLE ... MODIFY CHECK, and UPDATEs to
-- the new status values would fail the old constraint, so both steps
-- must happen together in a single table rebuild.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task',
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'pr_ready',
                                 'needs_lead_intervention', 'in_lead_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    merge_conflict_metadata TEXT,
    agent_type          TEXT,
    pr_url              TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type, pr_url
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    CASE status
        WHEN 'needs_pm_intervention' THEN 'needs_lead_intervention'
        WHEN 'in_pm_intervention'    THEN 'in_lead_intervention'
        ELSE status
    END,
    priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type, pr_url
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id    ON tasks(epic_id);
CREATE INDEX tasks_status     ON tasks(status);
CREATE INDEX tasks_priority   ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260321000001__rename_agent_roles_to_agents.sql ──
-- Rename agent_roles table to agents and update related structures.

ALTER TABLE agent_roles RENAME TO agents;

-- Recreate indexes with updated names.
DROP INDEX IF EXISTS agent_roles_project_id;
DROP INDEX IF EXISTS agent_roles_base_role;
DROP INDEX IF EXISTS agent_roles_is_default;

CREATE INDEX agents_project_id ON agents(project_id);
CREATE INDEX agents_base_role   ON agents(project_id, base_role);
CREATE INDEX agents_is_default  ON agents(project_id, is_default);

-- Rename role_id column in learned_prompt_history to agent_id.
ALTER TABLE learned_prompt_history RENAME COLUMN role_id TO agent_id;

DROP INDEX IF EXISTS learned_prompt_history_role_id;
CREATE INDEX learned_prompt_history_agent_id ON learned_prompt_history(agent_id);

-- ── V20260322000001__task_status_pr_states.sql ──
-- ADR-040: Expand PR state machine
-- Replace the single pr_ready status with a three-step pipeline:
--   approved  → task reviewer/lead has approved, waiting for PR creation
--   pr_draft  → PR created as draft, CI running
--   pr_review → PR out of draft, awaiting human code review / merge
--
-- Backward compat: any existing tasks still sitting in pr_ready are migrated
-- to pr_draft (the closest equivalent — PR exists, awaiting progression).

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS tasks_new;
CREATE TABLE tasks_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    epic_id             TEXT REFERENCES epics(id) ON DELETE SET NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    issue_type          TEXT NOT NULL DEFAULT 'task',
    status              TEXT NOT NULL DEFAULT 'open'
                             CHECK(status IN (
                                 'open', 'in_progress', 'verifying',
                                 'needs_task_review', 'in_task_review',
                                 'approved', 'pr_draft', 'pr_review',
                                 'needs_lead_intervention', 'in_lead_intervention',
                                 'closed'
                             )),
    priority            INTEGER NOT NULL DEFAULT 0,
    owner               TEXT NOT NULL DEFAULT '',
    labels              TEXT NOT NULL DEFAULT '[]',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    reopen_count        INTEGER NOT NULL DEFAULT 0,
    continuation_count  INTEGER NOT NULL DEFAULT 0,
    verification_failure_count INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    close_reason        TEXT,
    merge_commit_sha    TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    merge_conflict_metadata TEXT,
    agent_type          TEXT,
    pr_url              TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO tasks_new (
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    status, priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type, pr_url
)
SELECT
    id, project_id, short_id, epic_id, title, description, design, issue_type,
    CASE status
        WHEN 'pr_ready' THEN 'pr_draft'
        ELSE status
    END,
    priority, owner, labels, acceptance_criteria, reopen_count,
    continuation_count, verification_failure_count, created_at, updated_at,
    closed_at, close_reason, merge_commit_sha, memory_refs, merge_conflict_metadata,
    agent_type, pr_url
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id    ON tasks(epic_id);
CREATE INDEX tasks_status     ON tasks(status);
CREATE INDEX tasks_priority   ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;

-- ── V20260323000001__lifetime_counters_intervention_tracking.sql ──
-- Lifetime counters: monotonically increasing, never reset.
ALTER TABLE tasks ADD COLUMN total_reopen_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN total_verification_failure_count INTEGER NOT NULL DEFAULT 0;

-- Intervention tracking.
ALTER TABLE tasks ADD COLUMN intervention_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN last_intervention_at TEXT;

-- Backfill from current counters (may undercount due to prior resets).
UPDATE tasks SET
    total_reopen_count = reopen_count,
    total_verification_failure_count = verification_failure_count;

-- ── V20260324000001__rename_decomposition_to_planning.sql ──
-- ADR-042 §4a: Rename issue_type "decomposition" → "planning".
-- The broader "planning" type covers wave decomposition, epic metadata updates,
-- memory-ref attachment, and re-prioritization — all Planner work.
UPDATE tasks SET issue_type = 'planning' WHERE issue_type = 'decomposition';

-- ── V20260325000002__add_note_storage.sql ──
-- Add storage discriminator for note backing store.
ALTER TABLE notes ADD COLUMN storage TEXT NOT NULL DEFAULT 'file';

-- ── V20260325000003__add_consolidation_persistence.sql ──
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

-- ── V20260325000004__add_repo_map_cache.sql ──
CREATE TABLE repo_map_cache (
    project_id        TEXT NOT NULL,
    project_path      TEXT NOT NULL,
    worktree_path     TEXT,
    commit_sha        TEXT NOT NULL,
    rendered_map      TEXT NOT NULL,
    token_estimate    INTEGER NOT NULL,
    included_entries  INTEGER NOT NULL,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (project_id, project_path, worktree_path, commit_sha)
);

-- ── V20260326000001__add_note_content_hash.sql ──
-- Add content hash support for deterministic housekeeping/backfill.
ALTER TABLE notes ADD COLUMN content_hash TEXT;
CREATE INDEX notes_project_content_hash_idx ON notes(project_id, content_hash);

-- ── V20260327000001__epic_add_drafting_status.sql ──
-- Add 'drafting' status to epics. New epics default to 'drafting'.
-- Existing open epics remain open (no data migration needed).

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS epics_new;
CREATE TABLE epics_new (
    id          TEXT NOT NULL PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id    TEXT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    emoji       TEXT NOT NULL DEFAULT '',
    color       TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'drafting'
                     CHECK(status IN ('drafting', 'open', 'closed')),
    owner       TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at   TEXT,
    memory_refs TEXT NOT NULL DEFAULT '[]',
    UNIQUE(project_id, short_id)
);

INSERT INTO epics_new (
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs
)
SELECT
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs
FROM epics;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

PRAGMA foreign_keys = ON;

-- ── V20260331000001__add_verification_results.sql ──
-- Persisted verification step results so the frontend can load them on page open
-- instead of relying on transient SSE events.
CREATE TABLE IF NOT EXISTS verification_results (
    id          TEXT    NOT NULL PRIMARY KEY DEFAULT (lower(hex(randomblob(16)))),
    project_id  TEXT    NOT NULL,
    task_id     TEXT,
    run_id      TEXT    NOT NULL,
    phase       TEXT    NOT NULL CHECK (phase IN ('setup', 'verification')),
    step_index  INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    command     TEXT    NOT NULL DEFAULT '',
    exit_code   INTEGER NOT NULL,
    stdout      TEXT    NOT NULL DEFAULT '',
    stderr      TEXT    NOT NULL DEFAULT '',
    duration_ms INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_verification_results_task
    ON verification_results (task_id, created_at DESC);

CREATE INDEX idx_verification_results_project
    ON verification_results (project_id, created_at DESC);

CREATE INDEX idx_verification_results_run
    ON verification_results (run_id);

-- ── V20260331000002__add_repo_map_graph_artifact.sql ──
ALTER TABLE repo_map_cache ADD COLUMN graph_artifact TEXT;

-- ── V20260402000001__add_note_scope_paths.sql ──
-- Add scope_paths column for path-scoped knowledge injection.
-- JSON array of relative path prefixes where this note applies.
-- Empty array '[]' = global note (injected everywhere).
-- Example: '["server/crates/djinn-db", "server/crates/djinn-agent"]'
ALTER TABLE notes ADD COLUMN scope_paths TEXT NOT NULL DEFAULT '[]';

-- ── V20260407000001__add_repo_graph_cache.sql ──
-- ADR-050 §3 Chunk C: per-commit canonical SCIP graph cache.
--
-- Stores the serialized RepoDependencyGraph keyed by (project_id,
-- commit_sha).  This is a server-wide cache (no worktree dimension) — under
-- ADR-050 the graph is built once per `origin/main` commit by
-- `ensure_canonical_graph` and reused by every architect/chat session and
-- every worker dispatch until `origin/main` advances.
CREATE TABLE IF NOT EXISTS repo_graph_cache (
    project_id TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    graph_blob BLOB NOT NULL,
    built_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (project_id, commit_sha)
);

-- ── V20260407000002__notes_project_folder_title_idx.sql ──
-- Covering index for the hot `NoteRepository::list` / `::catalog` / scoped
-- search queries, which all shape as `WHERE project_id = ?1 ORDER BY folder,
-- title`. Without this index SQLite picks `notes_project_content_hash_idx` for
-- the equality probe and then falls back to `USE TEMP B-TREE FOR ORDER BY`,
-- which is what the `slow statement` warnings in the rotating logs track back
-- to. This composite index lets the planner serve both the filter and the
-- ordering from a single index walk.
CREATE INDEX IF NOT EXISTS notes_project_folder_title_idx
    ON notes(project_id, folder, title);

-- ── V20260408000001__epic_proposed_and_breakdown_fields.sql ──
-- ADR-051 Epic C — Proposal pipeline backend.
--
-- Adds:
--   1. A new 'proposed' epic status.  Epics in this state are architect
--      drafts that must not trigger auto-dispatch until explicitly
--      accepted (see coordinator::wave::maybe_create_planning_task and
--      the propose_adr_accept MCP tool).
--   2. auto_breakdown (INTEGER 0/1): when 0, epic_created no longer
--      triggers an automatic breakdown Planner dispatch.  Default 1 to
--      preserve existing behaviour.
--   3. originating_adr_id (TEXT, nullable): slug of the accepted ADR
--      that spawned this epic, threaded through into the breakdown
--      Planner's session context so downstream task creation inherits
--      the rationale.
--
-- SQLite does not support altering CHECK constraints in-place, so we
-- rebuild the table (mirroring V20260327000001__epic_add_drafting_status).

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS epics_new;
CREATE TABLE epics_new (
    id                  TEXT NOT NULL PRIMARY KEY,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    short_id            TEXT NOT NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    emoji               TEXT NOT NULL DEFAULT '',
    color               TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'drafting'
                             CHECK(status IN ('proposed', 'drafting', 'open', 'closed')),
    owner               TEXT NOT NULL DEFAULT '',
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at           TEXT,
    memory_refs         TEXT NOT NULL DEFAULT '[]',
    auto_breakdown      INTEGER NOT NULL DEFAULT 1 CHECK(auto_breakdown IN (0, 1)),
    originating_adr_id  TEXT,
    UNIQUE(project_id, short_id)
);

INSERT INTO epics_new (
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs,
    auto_breakdown, originating_adr_id
)
SELECT
    id, project_id, short_id, title, description, emoji, color,
    status, owner, created_at, updated_at, closed_at, memory_refs,
    1, NULL
FROM epics;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

PRAGMA foreign_keys = ON;

-- ── V20260409000001__remove_resolver_role.sql ──
-- Remove the dead "resolver" role. Conflict resolution now routes to Worker.

-- Delete any existing resolver agents (default or user-created).
DELETE FROM agents WHERE base_role = 'resolver';

-- Recreate table without "resolver" in the CHECK constraint.
-- SQLite does not support ALTER TABLE … DROP CONSTRAINT, so we rebuild.
CREATE TABLE agents_new (
    id                       TEXT NOT NULL PRIMARY KEY,
    project_id               TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name                     TEXT NOT NULL,
    base_role                TEXT NOT NULL
                                  CHECK(base_role IN (
                                      'worker', 'lead', 'planner',
                                      'architect', 'reviewer'
                                  )),
    description              TEXT NOT NULL DEFAULT '',
    system_prompt_extensions TEXT NOT NULL DEFAULT '',
    model_preference         TEXT,
    verification_command     TEXT,
    mcp_servers              TEXT NOT NULL DEFAULT '[]',
    skills                   TEXT NOT NULL DEFAULT '[]',
    is_default               INTEGER NOT NULL DEFAULT 0,
    created_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    learned_prompt           TEXT
);

INSERT INTO agents_new SELECT * FROM agents;
DROP TABLE agents;
ALTER TABLE agents_new RENAME TO agents;

CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name);

-- ── V20260413000001__add_note_embeddings.sql ──
-- Foundation for semantic note embeddings.
--
-- `note_embeddings` stores canonical embedding bytes and dimensions even when the
-- sqlite-vec extension is unavailable. The vec0 virtual table is created at
-- runtime during database initialization so startup can gracefully fall back.

CREATE TABLE note_embeddings (
    note_id        TEXT NOT NULL PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE,
    embedding      BLOB NOT NULL,
    embedding_dim  INTEGER NOT NULL,
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE note_embedding_meta (
    note_id         TEXT NOT NULL PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE,
    content_hash    TEXT NOT NULL,
    embedded_at     TEXT NOT NULL,
    model_version   TEXT NOT NULL,
    embedding_dim   INTEGER NOT NULL,
    extension_state TEXT NOT NULL DEFAULT 'pending',
    branch          TEXT NOT NULL DEFAULT 'main'
);

CREATE INDEX idx_note_embedding_meta_model_version
    ON note_embedding_meta(model_version);

CREATE INDEX idx_note_embedding_meta_embedded_at
    ON note_embedding_meta(embedded_at DESC);

CREATE INDEX idx_note_embedding_meta_branch
    ON note_embedding_meta(branch);

-- ── V20260413000002__add_agents_indexes.sql ──
-- Add missing indexes on the agents table.
-- These were originally appended to V20260409000001 after it had already been
-- applied, which broke refinery's checksum validation.

CREATE INDEX IF NOT EXISTS agents_project_id ON agents(project_id);
CREATE INDEX IF NOT EXISTS agents_base_role ON agents(project_id, base_role);
CREATE INDEX IF NOT EXISTS agents_is_default ON agents(project_id, is_default);
CREATE UNIQUE INDEX IF NOT EXISTS agents_one_default_per_base_role
    ON agents(project_id, base_role)
    WHERE is_default = 1;

-- ── V20260415000001__user_auth_sessions.sql ──
-- Web client GitHub OAuth session rows.
--
-- Separate from `sessions` (which tracks agent/task runs). Each row represents
-- a logged-in browser user session backed by a random 32-byte token stored in
-- the `djinn_session` cookie.

CREATE TABLE user_auth_sessions (
    token              TEXT NOT NULL PRIMARY KEY,
    user_id            TEXT NOT NULL,
    github_login       TEXT NOT NULL,
    github_name        TEXT,
    github_avatar_url  TEXT,
    github_access_token TEXT NOT NULL,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at         TEXT NOT NULL
);

CREATE INDEX idx_user_auth_sessions_user_id ON user_auth_sessions(user_id);
CREATE INDEX idx_user_auth_sessions_expires_at ON user_auth_sessions(expires_at);

-- ── V20260415000002__projects_installation_id.sql ──
-- V20260415000002__projects_installation_id.sql
--
-- SQLite counterpart of MySQL migration V4. Adds an optional installation_id
-- column to the `projects` table so GitHub-origin projects can record which
-- GitHub App installation grants access to the repo. Pre-existing rows leave
-- it NULL.

ALTER TABLE projects ADD COLUMN installation_id INTEGER;
