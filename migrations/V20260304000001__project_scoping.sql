-- Scope epics/tasks/sessions by project_id.
-- Also rename phase-review statuses to epic-review statuses.

PRAGMA foreign_keys = OFF;

-- Backfill strategy:
-- - If projects already exist, attach legacy rows to the first project.
-- - If no project exists and there is pre-scoped data, create one migration
--   project so project_id FKs can be satisfied.
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
    id, project_id, short_id, title, description, emoji, color, status,
    owner, created_at, updated_at, closed_at
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
    e.created_at,
    e.updated_at,
    e.closed_at
FROM epics e;

DROP TABLE epics;
ALTER TABLE epics_new RENAME TO epics;

CREATE INDEX epics_project_id ON epics(project_id);

-- TASKS -----------------------------------------------------------------------
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
                                 'draft', 'open', 'in_progress',
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
CREATE TABLE sessions_new (
    id            TEXT NOT NULL PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
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

DROP TABLE _migration_project;

PRAGMA foreign_keys = ON;
