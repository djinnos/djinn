-- Durable lifecycle ledger for graph-warm attempts. Unlike replace-set coverage
-- rows and Kubernetes Jobs, these rows retain the attempt history needed to
-- decide whether an exact revision can safely be recovered.
CREATE TABLE warm_graph_attempt (
    attempt_id  UUID        PRIMARY KEY,
    project_id  TEXT        NOT NULL,
    revision    TEXT        NOT NULL,
    status      VARCHAR(32) NOT NULL,
    started_at  TIMESTAMPTZ NOT NULL,
    deadline_at TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ,
    detail      TEXT,

    CONSTRAINT warm_graph_attempt_project_fk
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT warm_graph_attempt_revision_nonempty
        CHECK (btrim(revision) <> ''),
    CONSTRAINT warm_graph_attempt_status_valid
        CHECK (status IN (
            'running',
            'published_complete',
            'published_partial',
            'failed',
            'timed_out',
            'dispatch_failed'
        )),
    CONSTRAINT warm_graph_attempt_deadline_not_before_start
        CHECK (deadline_at >= started_at),
    CONSTRAINT warm_graph_attempt_finished_at_lifecycle
        CHECK (
            (status = 'running' AND finished_at IS NULL)
            OR (status <> 'running' AND finished_at IS NOT NULL)
        ),
    CONSTRAINT warm_graph_attempt_detail_bounded
        CHECK (detail IS NULL OR char_length(detail) <= 4096)
);

CREATE INDEX warm_graph_attempt_project_revision_started_at_idx
    ON warm_graph_attempt (project_id, revision, started_at DESC);
