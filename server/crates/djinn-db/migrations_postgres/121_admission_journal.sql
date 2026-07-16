-- Durable admission journal for task observation and build admission.
--
-- Rows are intentionally retained after becoming terminal.  A journal key
-- identifies one attempt generation, while `work_id` carries the durable work
-- identity owned by the coordinator or graph warmer.
CREATE TABLE admission_journal (
    domain                  VARCHAR(32)  NOT NULL,
    work_id                 VARCHAR(255) NOT NULL,
    generation              BIGINT       NOT NULL,
    workload_kind           VARCHAR(32)  NOT NULL,
    state                   VARCHAR(32)  NOT NULL,
    creator_server_epoch    VARCHAR(255) NOT NULL,
    object_name             VARCHAR(253) NOT NULL,
    object_uid              VARCHAR(255) NULL,
    created_at              TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ  NOT NULL DEFAULT now(),
    terminal_at             TIMESTAMPTZ  NULL,
    PRIMARY KEY (domain, work_id, generation),
    CONSTRAINT admission_journal_generation_positive CHECK (generation >= 0),
    CONSTRAINT admission_journal_domain_check CHECK (
        domain IN ('task_observation', 'warm_build', 'invocation_build')
    ),
    CONSTRAINT admission_journal_workload_kind_check CHECK (
        workload_kind IN ('task', 'warm', 'invocation')
    ),
    CONSTRAINT admission_journal_state_check CHECK (
        state IN ('reserved', 'create_in_flight', 'create_unknown', 'live', 'terminal')
    ),
    CONSTRAINT admission_journal_terminal_timestamp_check CHECK (
        (state = 'terminal' AND terminal_at IS NOT NULL)
        OR (state <> 'terminal' AND terminal_at IS NULL)
    )
);

-- Occupancy is selected by state and domain by the repository.  This index
-- keeps the cap check and recovery scans narrow without deleting audit rows.
CREATE INDEX admission_journal_occupancy_idx
    ON admission_journal (domain, state)
    WHERE state IN ('reserved', 'create_in_flight', 'create_unknown', 'live');

CREATE INDEX admission_journal_work_history_idx
    ON admission_journal (domain, work_id, generation DESC);
