-- Phase 1, Step 4 — proposal roles.
--
-- A coarse capability role for the proposal workflow:
--   proposer (default for new sign-ups) → pm → engineer
-- `is_admin` stays an orthogonal superset. Existing users keep their current
-- power (they can already create/kick off work), so back-fill them to engineer;
-- only future sign-ups default to proposer (least privilege).

ALTER TABLE users ADD COLUMN role VARCHAR(16) NOT NULL DEFAULT 'proposer';

UPDATE users SET role = 'engineer';
