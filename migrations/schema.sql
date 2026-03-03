-- Canonical schema — ground truth. Matches result of running all migrations.
-- Updated manually after each migration is added.
-- Last updated: V20260303000011__task_merge_commit_sha.sql

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
                                 'draft', 'open', 'in_progress',
                                 'needs_task_review', 'in_task_review',
                                 'needs_phase_review', 'in_phase_review',
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
    merge_commit_sha    TEXT
);

CREATE INDEX tasks_epic_id ON tasks(epic_id);
CREATE INDEX tasks_status   ON tasks(status);
CREATE INDEX tasks_priority ON tasks(priority, created_at);

CREATE TABLE blockers (
    task_id          TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
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
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX activity_log_task_id ON activity_log(task_id);

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
