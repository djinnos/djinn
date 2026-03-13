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
