-- The invocation half of "compare-and-swap fenced by invocation and Pod UID".
--
-- Migration 164 shipped the six-state resize lifecycle and fenced every
-- transition on `(task_run_id, permit_id, fencing_token, pod_uid, state)`. That
-- is the Pod half of the fence. The invocation half was NOT expressible:
-- `build_pod_permits.task_run_id` is the PRIMARY KEY, so there is exactly one
-- row per task run, and neither 162 nor 164 carried an invocation column. A task
-- run executes MANY invocations, each of which crosses the CPU threshold, lifts
-- and drops on that single row. The `drop_applying -> birth_confirmed` back-edge
-- makes the CYCLE expressible; nothing made the CURRENT OCCUPANT of the cycle
-- identifiable. This migration is what makes it identifiable.
--
-- The identity stored is the durable invocation-lease consumer id
-- (`build_leases.consumer_id` for `consumer_kind = 'task_invocation'`), which is
-- the same string the worker presents on `queue_lease`/`release_lease`. It is
-- deliberately NOT a fresh surrogate: a fence nobody outside this table can
-- name is a fence no caller can present, and the whole point is that the drop
-- issued for invocation N is rejected when invocation N+1 owns the row.
ALTER TABLE build_pod_permits
    ADD COLUMN resize_invocation_id VARCHAR(255);

ALTER TABLE build_pod_permits
    -- An empty string is not an invocation. Without this a caller that fenced on
    -- `''` would match a row that had never been claimed at all.
    ADD CONSTRAINT build_pod_permits_resize_invocation_shape_check CHECK (
        resize_invocation_id IS NULL OR length(btrim(resize_invocation_id)) > 0
    ),
    -- The two states that can only be reached by claiming an invocation must
    -- carry one. `drop_required` and `quarantined` are deliberately excluded:
    -- both are reachable directly from `birth_confirmed` by an infrastructure
    -- observer (a Pod that died before any invocation ever lifted), and that
    -- path has no invocation to name.
    ADD CONSTRAINT build_pod_permits_resize_invocation_state_check CHECK (
        state NOT IN ('lift_applying', 'lifted') OR resize_invocation_id IS NOT NULL
    );

-- Migration 164's trigger body, plus the invocation write-window rule.
--
-- `resize_invocation_id` is not write-once — it MUST change, once per invocation,
-- or a task run's second lift could never be told from its first. It is instead
-- write-windowed: it may only change on the single edge that starts a lift,
-- `birth_confirmed -> lift_applying`. Every other edge in the graph carries it
-- forward unchanged, so a drop, a quarantine or a return to `birth_confirmed`
-- cannot silently re-key the row underneath a caller that is mid-sequence.
CREATE OR REPLACE FUNCTION build_pod_permits_prevent_immutable_mutation() RETURNS trigger AS $$
BEGIN
    IF NEW.task_run_id <> OLD.task_run_id OR NEW.permit_id <> OLD.permit_id
       OR NEW.fencing_token <> OLD.fencing_token
       OR (OLD.job_uid IS NOT NULL AND NEW.job_uid IS DISTINCT FROM OLD.job_uid)
       OR (OLD.pod_uid IS NOT NULL AND (NEW.pod_namespace, NEW.pod_name, NEW.pod_uid,
           NEW.launcher_container_name, NEW.launcher_container_id, NEW.image_digest,
           NEW.observed_launcher_protocol, NEW.effective_launcher_protocol, NEW.admitted_cpu_millicores)
           IS DISTINCT FROM (OLD.pod_namespace, OLD.pod_name, OLD.pod_uid,
           OLD.launcher_container_name, OLD.launcher_container_id, OLD.image_digest,
           OLD.observed_launcher_protocol, OLD.effective_launcher_protocol, OLD.admitted_cpu_millicores)) THEN
        RAISE EXCEPTION 'build pod permit immutable identity or fencing token changed';
    END IF;
    IF NEW.resize_invocation_id IS DISTINCT FROM OLD.resize_invocation_id
       AND NOT (OLD.state = 'birth_confirmed' AND NEW.state = 'lift_applying') THEN
        RAISE EXCEPTION 'build pod permit resize invocation id may only change on entry to lift_applying';
    END IF;
    IF NOT ((OLD.state = 'acquired' AND NEW.state IN ('job_created', 'released'))
        OR (OLD.state = 'job_created' AND NEW.state IN ('birth_confirmed', 'released'))
        OR (OLD.state = 'birth_confirmed' AND NEW.state IN ('lift_applying', 'drop_required', 'quarantined', 'released'))
        OR (OLD.state = 'lift_applying' AND NEW.state IN ('lifted', 'drop_required', 'quarantined', 'released'))
        OR (OLD.state = 'lifted' AND NEW.state IN ('drop_required', 'quarantined', 'released'))
        OR (OLD.state = 'drop_required' AND NEW.state IN ('drop_applying', 'quarantined', 'released'))
        OR (OLD.state = 'drop_applying' AND NEW.state IN ('birth_confirmed', 'quarantined', 'released'))
        OR (OLD.state = 'quarantined' AND NEW.state = 'released')
        OR (OLD.state = NEW.state)) THEN
        RAISE EXCEPTION 'illegal build pod resize lifecycle transition';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
