-- Durable record of the latest build-admission denial per dispatch consumer.
--
-- `BuildAdmissionDecision::Denied` has carried a `DenialCause` since #2661, and
-- that value was only ever LOGGED: `dispatch/task_dispatch.rs` emitted one
-- `tracing::info!` line and returned `Err(())`. Nothing wrote it anywhere, so
-- no projection could join a real reason to a stranded task.
--
-- On 2026-07-29 the board stopped dispatching for five hours. Every tick logged
-- `cause: "controller_not_admitting"`. `board_health` reported `gate_verdict:
-- "unexplained"` with `reasons: []`, once every thirty seconds, because the one
-- authority that knew the answer had thrown it away. The only surviving copy
-- was in container logs on the node.
--
-- The alternative — an MCP tool reading the controller's live readiness — does
-- not work. Readiness is process-local state on the leader, `mark_topology_ready`
-- is reachable only from `become_leader`, and a standby answering the call would
-- always report `TopologyPending`: a confident wrong answer. A durable signal
-- written by whoever actually made the decision is the honest shape.
--
-- One row per consumer, upserted on each denial and DELETED the moment the same
-- consumer is permitted. Bounded by the number of live dispatch consumers, and
-- deletion-on-permit is what stops this table becoming the #2661 tombstone in a
-- new location: a denial row that outlives its condition is exactly the failure
-- being fixed.
CREATE TABLE build_admission_denials (
    -- Matches `build_leases.consumer_kind`; today only 'task_dispatch' writes.
    consumer_kind    VARCHAR(32)  NOT NULL,
    -- The task id for 'task_dispatch'. NOT the `{task_id}:{generation}` lease
    -- consumer id: a denial is about the task, and the generation moves under it.
    consumer_id      VARCHAR(255) NOT NULL,
    -- `DenialCause`, rendered by its `Display`.
    cause            VARCHAR(32)  NOT NULL,
    -- `BuildAdmissionReadiness::as_str()` when the cause is
    -- `controller_not_admitting`. This is the field that existed ONLY in
    -- container logs during the outage.
    readiness        VARCHAR(64)  NULL,
    -- The capacity authority's own words for `authority_unavailable`.
    detail           TEXT         NULL,
    -- Occupancy as MEASURED. NULL means the denial never consulted it, which is
    -- true of every readiness denial. It is deliberately not 0: a fabricated
    -- zero is what made a tombstoned lease look like a full pool (#2661).
    occupancy        BIGINT       NULL,
    -- The cap that WOULD have been enforced, resolved from the authority.
    cap              BIGINT       NOT NULL,
    -- The deciding process's admission epoch, so a denial can be attributed to
    -- a specific incarnation and compared against `admission_journal`.
    server_epoch     VARCHAR(255) NOT NULL,
    -- Start of the current uninterrupted denial streak. Reset only by the
    -- DELETE on permit, so "denied since" survives repeated ticks.
    first_denied_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    denied_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    denial_count     BIGINT       NOT NULL DEFAULT 1,
    PRIMARY KEY (consumer_kind, consumer_id),
    CONSTRAINT build_admission_denials_cause_check CHECK (
        cause IN ('at_capacity', 'controller_not_admitting', 'authority_unavailable')
    ),
    CONSTRAINT build_admission_denials_count_positive CHECK (denial_count > 0),
    CONSTRAINT build_admission_denials_streak_ordered CHECK (first_denied_at <= denied_at)
);

-- Board-wide reads ("what is denying everything right now") scan by recency.
CREATE INDEX build_admission_denials_recent_idx
    ON build_admission_denials (denied_at DESC);
