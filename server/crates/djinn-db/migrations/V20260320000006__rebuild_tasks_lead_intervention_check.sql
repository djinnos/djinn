-- ADR-034 §1: update tasks CHECK constraint to use 'needs_lead_intervention' /
-- 'in_lead_intervention' in place of the old PM names.
--
-- The previous migration (V20260320000005) updated the data values but SQLite
-- does not support ALTER TABLE ... MODIFY CHECK, so the table must be rebuilt
-- to pick up the new constraint.

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
CREATE INDEX tasks_epic_id    ON tasks(epic_id);
CREATE INDEX tasks_status     ON tasks(status);
CREATE INDEX tasks_priority   ON tasks(priority, created_at);

PRAGMA foreign_keys = ON;
