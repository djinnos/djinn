-- Unify build-slot capacity accounting onto the v1 lease ledger.
--
-- Before this migration two authorities each enforced DJINN_MAX_BUILD_TASKRUNS
-- over DISJOINT populations: the v0 admission journal governed task-runs
-- (domain task_observation) while this ledger governed graph warming. At
-- enforce that yields 2x the operator's configured concurrency. Capacity
-- accounting now lives here alone; `admission_journal` is retained purely as
-- the lifecycle/fencing ledger (UID + generation lifecycle, restart recovery,
-- and the reclamation added in #2597).
--
-- Two schema changes make that possible:
--
-- 1. `task_dispatch` joins the consumer kinds. It is the layer-1 dispatch
--    admission unit -- the coarse, role-derived reservation taken before a
--    task-run pod is created, for work CERTAIN to compile. `task_invocation`
--    remains the layer-2, role-agnostic, measured escalation unit.
--
-- 2. `weight` makes occupancy a SUM rather than a COUNT. Two things need it:
--
--    * The non-double-charge rule. A build-capable task-run reserves a
--      `task_dispatch` unit at spawn, and the compile it then runs escalates
--      to a `task_invocation` lease. Those are ONE physical compile and must
--      be charged ONCE. The invocation lease of a task-run that already holds
--      an occupying dispatch row is therefore recorded at weight 0: it still
--      receives a fencing token and the launcher quota lift, it simply does
--      not buy capacity twice. A Light task-run holds no dispatch row, so its
--      invocation lease carries full weight -- it contends only if it really
--      compiles.
--
--    * Non-uniform CPU. Weight is derived from the rendered manifests
--      (`ceil(cpu_millicores / launcher_leased_millicores)`), never hand-set,
--      so raising DJINN_K8S_WARM_CPU_REQUEST reweights warm automatically.
--      On the default render a warm Job and a leased task invocation BOTH
--      request 4000m, so both weigh exactly 1 unit.
--
-- Existing rows are weight 1, which is exactly what the previous COUNT(*)
-- occupancy meant for them, so the migration is occupancy-preserving.

ALTER TABLE build_leases
    ADD COLUMN weight BIGINT NOT NULL DEFAULT 1;

ALTER TABLE build_leases
    ADD CONSTRAINT build_leases_weight_check CHECK (weight >= 0);

ALTER TABLE build_leases
    DROP CONSTRAINT build_leases_consumer_kind_check;

ALTER TABLE build_leases
    ADD CONSTRAINT build_leases_consumer_kind_check
    CHECK (consumer_kind IN ('task_invocation', 'graph_warm', 'task_dispatch'));

-- Occupancy is now SUM(weight) over the occupying states. Carry `weight` in
-- the partial index so the capacity sum is answered from the index alone.
DROP INDEX build_leases_occupied_idx;
CREATE INDEX build_leases_occupied_idx
    ON build_leases (state, weight)
    WHERE state IN ('granted', 'launching', 'bound', 'active', 'suspect');

-- Weight is immutable for the life of a row, exactly like the identity fields:
-- capacity that was granted at one weight may never be silently re-priced
-- underneath an occupant.
CREATE OR REPLACE FUNCTION build_leases_prevent_immutable_mutation() RETURNS trigger AS $$
BEGIN
    IF NEW.consumer_kind <> OLD.consumer_kind OR NEW.consumer_id <> OLD.consumer_id
       OR NEW.immutable_identity <> OLD.immutable_identity
       OR NEW.weight <> OLD.weight
       OR (OLD.bound_pod_uid IS NOT NULL AND NEW.bound_pod_uid IS DISTINCT FROM OLD.bound_pod_uid) THEN
        RAISE EXCEPTION 'build lease immutable identity, weight or pod UID changed';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
