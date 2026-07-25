-- Durable run-dir ledger for disk-aware build admission (proposal nquz, phase 1).
--
-- This migration is purely additive: it introduces the run-dir ledger and does
-- NOT modify any existing admission_journal / admission_handoff row. In this
-- phase the ledger ships DARK/OBSERVE — nothing writes production rows through
-- it yet, eager cargo-target seeding stays enabled, and no GC deletion path is
-- wired to it.
--
-- A row is keyed by (volume_id, pod_uid) and carries the lease-coupled
-- run-dir lifecycle. Generations are compared-and-set by the repository under a
-- per-volume advisory lock plus `SELECT FOR UPDATE` on the row itself, mirroring
-- the admission_journal serialization conventions.
CREATE TABLE run_dirs (
    volume_id        VARCHAR(255) NOT NULL,
    pod_uid          VARCHAR(255) NOT NULL,
    task_run_id      VARCHAR(255) NULL,
    project_id       VARCHAR(255) NULL,
    base_fingerprint VARCHAR(255) NULL,
    state            VARCHAR(24)  NOT NULL,
    generation       BIGINT       NOT NULL DEFAULT 0,
    reserved_bytes   BIGINT       NOT NULL DEFAULT 0,
    measured_bytes   BIGINT       NOT NULL DEFAULT 0,
    quota_id         VARCHAR(255) NULL,
    last_lease_at    TIMESTAMPTZ  NULL,
    temp_path        TEXT         NULL,
    final_path       TEXT         NULL,
    created_at       TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (volume_id, pod_uid),
    CONSTRAINT run_dirs_generation_nonneg CHECK (generation >= 0),
    CONSTRAINT run_dirs_reserved_bytes_nonneg CHECK (reserved_bytes >= 0),
    CONSTRAINT run_dirs_measured_bytes_nonneg CHECK (measured_bytes >= 0),
    -- The eight lease-lifecycle states plus the reconciliation-only
    -- `quarantined_unowned` bucket. Quarantined dirs count against observed
    -- physical bytes but are never an automated deletion candidate.
    CONSTRAINT run_dirs_state_check CHECK (
        state IN (
            'absent',
            'reserved',
            'seeding',
            'ready_active',
            'ready_idle',
            'reclaimable',
            'reclaiming',
            'quarantined_unowned'
        )
    )
);

-- Volume/state aggregation drives GC ordering and bounded telemetry rollups.
CREATE INDEX run_dirs_volume_state_idx
    ON run_dirs (volume_id, state);

-- Terminal-callback reconciliation resolves a run row by its runtime task-run
-- UID; only non-null bindings are ever queried this way.
CREATE INDEX run_dirs_task_run_idx
    ON run_dirs (task_run_id)
    WHERE task_run_id IS NOT NULL;

-- The seed byte projection consults the last successful measurement for the
-- same project and base fingerprint.
CREATE INDEX run_dirs_project_fingerprint_idx
    ON run_dirs (project_id, base_fingerprint)
    WHERE project_id IS NOT NULL AND base_fingerprint IS NOT NULL;
