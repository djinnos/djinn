-- Migration 188: durable readiness run creator attribution.
--
-- Readiness kickoff no longer requires the caller to be the project's GitHub
-- owner, so the run row must record which authenticated user started it.
-- Token and cost accounting follow that user, not the repository owner.
--
-- Additive and nullable: runs materialized before this migration have no
-- recorded starter and stay readable.
ALTER TABLE readiness_runs ADD COLUMN created_by_user_id VARCHAR(36) NULL;
CREATE INDEX readiness_runs_created_by_user_idx ON readiness_runs(created_by_user_id);
