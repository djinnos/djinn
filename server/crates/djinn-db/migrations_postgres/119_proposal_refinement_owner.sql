-- Durable owner for a refinement run. Nullable for proposals created before
-- this expand migration; new starts must populate it transactionally.
ALTER TABLE proposals
    ADD COLUMN refinement_owner_user_id VARCHAR(36) NULL
    REFERENCES users(id) ON DELETE RESTRICT;

CREATE INDEX proposals_refinement_owner_user_id
    ON proposals(refinement_owner_user_id)
    WHERE refinement_owner_user_id IS NOT NULL;
