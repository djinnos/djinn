-- Canonical schema — ground truth. Matches result of running all migrations.
-- Updated manually after each migration is added.
-- Last updated: V20260309000004__activity_log_archived.sql

CREATE TABLE settings (
    key        TEXT NOT NULL PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE projects (
    id                    TEXT NOT NULL PRIMARY KEY,
    name                  TEXT NOT NULL UNIQUE,
    path                  TEXT NOT NULL UNIQUE,
    created_at            TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    setup_commands        TEXT NOT NULL DEFAULT '[]',
    verification_commands TEXT NOT NULL DEFAULT '[]',
  target_branch TEXT NOT NULL DEFAULT 'main',
  auto_merge INTEGER NOT NULL DEFAULT 1,
  sync_enabled INTEGER NOT NULL DEFAULT 0,
  sync_remote TEXT
);

CREATE TABLE epics (
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
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at   TEXT,
    UNIQUE(project_id, short_id)
);

CREATE INDEX epics_project_id ON epics(project_id);

CREATE TABLE tasks (
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
                                 'draft', 'open', 'in_progress', 'verifying',
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

CREATE INDEX tasks_project_id ON tasks(project_id);
CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status   ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

CREATE TABLE blockers (
    task_id          TEXT REFERENCES tasks(id) ON DELETE CASCADE,
    blocking_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, blocking_task_id)
);

CREATE TABLE activity_log (
    id          TEXT NOT NULL PRIMARY KEY,
    task_id     TEXT,
    actor_id    TEXT NOT NULL DEFAULT '',
    actor_role  TEXT NOT NULL DEFAULT '',
    event_type  TEXT NOT NULL,
    payload     TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    archived    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX activity_log_task_id ON activity_log(task_id);

CREATE TABLE sessions (
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

CREATE INDEX sessions_project_id ON sessions(project_id);
CREATE INDEX sessions_task_id ON sessions(task_id);
CREATE INDEX sessions_status ON sessions(status);
CREATE INDEX sessions_project_agent_status ON sessions(project_id, agent_type, status);
