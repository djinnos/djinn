-- Durable projection of Kueue Workload admission state onto task-runs.
--
-- Kueue cutover slice S5 (task kh7g, proposal 9oga). This is the relation that
-- replaces the deleted `admission_journal` — with one structural difference that
-- is the entire point of the cutover.
--
-- `admission_journal` was an AUTHORITY: it reserved capacity before the object
-- that consumed it existed, and everything downstream (settle windows, absence
-- proofs, CAS fencing) was compensation for accounting rows no Kubernetes object
-- owned. This table is a PROJECTION: Kueue's ClusterQueue decides, this records
-- what it decided. Every row is derived from a live Workload observed on a
-- watch, so a row can never outlive the object it describes — deleting the
-- Workload deletes the row.
--
-- ## Why a task-run needs this at all
--
-- Under create-then-admit a build-capable Job is created `suspend: true` and no
-- Pod exists until Kueue admits it. The in-pod supervisor is what creates the
-- `task_runs` row, so between dispatch and admission djinn has NOTHING that says
-- why a task is sitting still: no run, no session, no denial. That is exactly
-- the shape of the 2026-07-29 outage, where the board reported `unexplained`
-- with empty reasons for five hours. This row is the answer to "what is this
-- task waiting for", written by the process that watched Kueue decide it.
--
-- ## Reversible by construction
--
-- `admission` moves BOTH ways. A preempted Workload returns to 'pending' with
-- Kueue's own reason ('Preempted', 'ClusterQueueStopped', 'Deactivated'), and a
-- re-admitted one moves back to 'admitted'. A one-way column would have made a
-- quota eviction unrepresentable, and an unrepresentable eviction reads as a
-- permanently-admitted run that nothing will ever finish.
--
-- ## Bounded
--
-- One row per task-run with a live Workload. `transitions` counts observed state
-- CHANGES, not observations: a watch reconnect replays every Workload it knows
-- about, and a projection that counted replays would inflate without bound and
-- make "how often is this queue thrashing" unreadable.
CREATE TABLE kueue_workload_admission (
    -- The `task_runs.id` this Workload accounts for, resolved from the Job's
    -- `djinn.app/task-run-id` label or its `djinn-taskrun-<uuid>` name. A
    -- Workload that resolves to neither is not a task-run and writes nothing.
    --
    -- Deliberately NOT a foreign key to `task_runs`: the whole reason this table
    -- exists is the window BEFORE the in-pod supervisor creates that row.
    task_run_id   VARCHAR(64)  PRIMARY KEY,
    -- `WorkloadAdmission::as_str()`. Reversible between 'pending' and 'admitted'.
    admission     VARCHAR(32)  NOT NULL,
    -- Kueue's own word for the current state, when it gave one. NULL is honest
    -- ("Kueue offered no reason"), never a stand-in for a reason we lost.
    reason        TEXT         NULL,
    -- The observed Workload's object name, so an operator can go straight to
    -- `kubectl -n djinn describe workload <name>`.
    workload_name VARCHAR(253) NULL,
    first_seen_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    observed_at   TIMESTAMPTZ  NOT NULL DEFAULT now(),
    -- Observed state CHANGES since `first_seen_at`. A resync must not move this.
    transitions   BIGINT       NOT NULL DEFAULT 0,
    CONSTRAINT kueue_workload_admission_state_check CHECK (
        admission IN ('pending', 'admitted', 'finished')
    ),
    CONSTRAINT kueue_workload_admission_transitions_nonneg CHECK (transitions >= 0),
    CONSTRAINT kueue_workload_admission_observed_ordered CHECK (first_seen_at <= observed_at)
);

-- "What is queued right now" is the operator question this exists to answer.
CREATE INDEX kueue_workload_admission_state_idx
    ON kueue_workload_admission (admission, observed_at DESC);
