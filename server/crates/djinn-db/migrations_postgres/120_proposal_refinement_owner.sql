-- Durable attribution for proposal refinement. Nullable in the expand phase:
-- legacy missing owners fail closed rather than falling back across users.
ALTER TABLE proposals
    ADD COLUMN refinement_owner_user_id VARCHAR(36) NULL REFERENCES users(id) ON DELETE SET NULL;

CREATE INDEX proposals_refinement_owner_user_id
    ON proposals(refinement_owner_user_id)
    WHERE refinement_owner_user_id IS NOT NULL;
