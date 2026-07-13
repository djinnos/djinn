-- Immutable, project-scoped audit ledger for note mutations. `note_id` deliberately
-- has no FK to `notes`: history must remain available after a note is deleted.
CREATE TABLE note_revision_events (
    id                VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id        VARCHAR(36) NOT NULL,
    note_id           VARCHAR(36) NULL,
    note_seq          BIGINT NULL,
    event_kind        VARCHAR(32) NOT NULL,
    content_before    TEXT NULL,
    content_after     TEXT NULL,
    confidence_before DOUBLE PRECISION NULL,
    confidence_after  DOUBLE PRECISION NULL,
    actor_kind        VARCHAR(16) NOT NULL,
    actor_id          VARCHAR(255) NULL,
    subsystem         VARCHAR(128) NULL,
    session_id        VARCHAR(36) NULL,
    task_id           VARCHAR(36) NULL,
    task_run_id       VARCHAR(36) NULL,
    reason            TEXT NOT NULL,
    created_at        VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),

    CONSTRAINT fk_note_revision_events_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT chk_note_revision_events_id_uuid
        CHECK (id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    CONSTRAINT chk_note_revision_events_kind
        CHECK (event_kind IN ('created', 'updated', 'deleted', 'confidence_changed', 'extraction_skipped')),
    CONSTRAINT chk_note_revision_events_actor_kind
        CHECK (actor_kind IN ('human', 'agent', 'system')),
    CONSTRAINT chk_note_revision_events_note_identity
        CHECK ((note_id IS NULL) = (note_seq IS NULL) AND (note_seq IS NULL OR note_seq > 0)),
    CONSTRAINT chk_note_revision_events_reason_trimmed
        CHECK (
            reason ~ '[^[:space:]]'
            AND reason !~ '^[[:space:]]'
            AND reason !~ '[[:space:]]$'
        ),
    CONSTRAINT chk_note_revision_events_actor_attribution
        CHECK (
            (actor_kind IN ('human', 'agent') AND actor_id IS NOT NULL AND length(btrim(actor_id)) > 0 AND subsystem IS NULL)
            OR (actor_kind = 'system' AND actor_id IS NULL AND subsystem IS NOT NULL AND length(btrim(subsystem)) > 0)
        ),
    CONSTRAINT chk_note_revision_events_shape
        CHECK (
            (event_kind = 'created'
                AND note_id IS NOT NULL
                AND content_before IS NULL AND confidence_before IS NULL
                AND content_after IS NOT NULL AND confidence_after IS NOT NULL)
            OR (event_kind = 'updated'
                AND note_id IS NOT NULL
                AND content_before IS NOT NULL AND confidence_before IS NOT NULL
                AND content_after IS NOT NULL AND confidence_after IS NOT NULL)
            OR (event_kind = 'deleted'
                AND note_id IS NOT NULL
                AND content_before IS NOT NULL AND confidence_before IS NOT NULL
                AND content_after IS NULL AND confidence_after IS NULL)
            OR (event_kind = 'confidence_changed'
                AND note_id IS NOT NULL
                AND content_before IS NULL AND content_after IS NULL
                AND confidence_before IS NOT NULL AND confidence_after IS NOT NULL)
            OR (event_kind = 'extraction_skipped'
                AND note_id IS NULL
                AND content_before IS NULL AND content_after IS NULL
                AND confidence_before IS NULL AND confidence_after IS NULL
                AND (session_id IS NOT NULL OR task_run_id IS NOT NULL))
        )
);

-- Partial uniqueness leaves extraction-skipped events outside note history.
CREATE UNIQUE INDEX note_revision_events_project_note_seq_unique
    ON note_revision_events(project_id, note_id, note_seq)
    WHERE note_id IS NOT NULL;
CREATE INDEX note_revision_events_project_note_history
    ON note_revision_events(project_id, note_id, note_seq);
CREATE INDEX note_revision_events_project_session_cursor
    ON note_revision_events(project_id, session_id, created_at, id)
    WHERE session_id IS NOT NULL;
CREATE INDEX note_revision_events_project_task_cursor
    ON note_revision_events(project_id, task_id, created_at, id)
    WHERE task_id IS NOT NULL;
CREATE INDEX note_revision_events_project_task_run_cursor
    ON note_revision_events(project_id, task_run_id, created_at, id)
    WHERE task_run_id IS NOT NULL;
