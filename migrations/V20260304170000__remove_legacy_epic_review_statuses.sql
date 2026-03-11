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
