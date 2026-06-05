-- Stop / Abort / Freeze a proposal build.
--
-- `build_frozen` is a flag orthogonal to `status`: a frozen build stays
-- `building` but its tasks are held out of dispatch (see build_ready_where).
-- A boolean (not a new status) keeps reconcile_approval and the status
-- allowlist untouched.
--
-- `build_breakdown_task_id` is the 1:1 link to the epic_breakdown task created
-- at graduation, so a stop can find and force-close it. ON DELETE SET NULL
-- keeps the proposal row safe if the task is hard-deleted.
ALTER TABLE proposals ADD COLUMN build_frozen BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE proposals ADD COLUMN build_breakdown_task_id VARCHAR(36) NULL
    REFERENCES tasks(id) ON DELETE SET NULL;
