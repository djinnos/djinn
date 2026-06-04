-- Phase 1, Step 5 — graduation (kick-off → build).
--
-- When an approved proposal is kicked off, it graduates into one epic per
-- primary target project (the existing single-repo-write execution engine).
-- `proposal_epics` links the proposal to the epics it spawned; `build_owner`
-- is the participant accountable for the build (also the epic creator, so
-- commits attribute correctly).

ALTER TABLE proposals ADD COLUMN build_owner_user_id VARCHAR(36) NULL;

CREATE TABLE proposal_epics (
    proposal_id VARCHAR(36) NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    epic_id     VARCHAR(36) NOT NULL REFERENCES epics(id) ON DELETE CASCADE,
    project_id  VARCHAR(36) NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_at  VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (proposal_id, epic_id)
);

CREATE INDEX proposal_epics_proposal_id ON proposal_epics(proposal_id);
