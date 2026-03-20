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
