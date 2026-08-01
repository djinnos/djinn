-- When this permit's resize lifecycle last moved, so "somebody is still doing
-- this" becomes expressible.
--
-- # The failure this exists to make impossible
--
-- `task_run_resize_reconcile` classifies `drop_required` and `drop_applying` as
-- states "no live driver rests in" and resumes them unconditionally. That
-- premise is false. A LIVE worker's own drop TRANSITS both of them:
-- `require_drop` writes `lifted -> drop_required`, `apply_drop` writes
-- `drop_required -> drop_applying`, and the row then sits at `drop_applying`
-- for as long as the kubelet takes to actuate the downsize — about a second.
-- The reconciler sweeps every 30 seconds, so roughly one in thirty of those
-- windows is landed in. Production on 2026-08-01 landed in 6 of 12.
--
-- Both drivers then race `drop_applying -> birth_confirmed`. The loser's
-- compare-and-swap is rejected, its drop settles as `Unavailable`, and
-- `release_lease` answers `LeaseUnavailable` for a drop that SUCCEEDED. Three
-- of those in a row force a cancel, so a healthy task run is killed because the
-- reconciler raced its own worker.
--
-- Distinguishing the two cases needs one fact the table did not carry: how long
-- the row has been where it is. Neither `acquired_at` (fixed at insert) nor
-- `released_at` (written once, terminally) moves when the state does.
--
-- # Why a backfill of `now()` rather than `acquired_at`
--
-- Every pre-existing row gets the migration's own timestamp, which claims the
-- state changed at deploy time. That is deliberately the CONSERVATIVE lie:
-- `acquired_at` would UNDER-estimate `state_changed_at` for any row that has
-- transitioned since insert, which OVER-estimates its age and makes a live
-- worker's in-flight drop look abandoned — the exact defect, re-introduced by
-- the backfill. Over-estimating `state_changed_at` costs at most one grace
-- window of delayed reconciliation, once, for rows that were already stranded;
-- and a row genuinely abandoned by a dead worker is still reconciled
-- immediately on the owner-gone and pod-absent facts, which do not consult this
-- column at all.
ALTER TABLE build_pod_permits
    ADD COLUMN state_changed_at TIMESTAMPTZ NOT NULL DEFAULT now();

-- Migration 168's trigger body, plus the stamp.
--
-- The stamp lives in the trigger and not in the repository's UPDATE statements
-- for one reason: the trigger is the only place that sees EVERY write. A column
-- maintained by each caller is a column the next caller forgets, and a
-- forgotten stamp reads as "this row has not moved in hours" — which is
-- precisely the input the reconciler is about to act on. Here, `NEW.state`
-- differing from `OLD.state` is sufficient and no caller can opt out: the
-- assignment overwrites whatever the statement supplied.
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
    -- A SELF-transition deliberately does not restamp. `drop_applying ->
    -- drop_applying` is what a worker's retried `release_lease` writes, and
    -- restamping there would let a worker that is stuck renew its own grace
    -- forever — the row would never age out and a genuinely wedged drop would
    -- never be reconciled.
    IF NEW.state IS DISTINCT FROM OLD.state THEN
        NEW.state_changed_at := now();
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
