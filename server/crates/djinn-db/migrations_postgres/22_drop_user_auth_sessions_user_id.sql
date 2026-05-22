-- Cut `user_auth_sessions` over to the joined identity model.
--
-- Phase 1 stuffed the GitHub numeric id (as a string) into a denormalised
-- `user_id` column. Phase 2 added `user_fk` pointing at `users.id`, and
-- every authenticated path is supposed to read identity through the join.
--
-- Cut-over:
--   1. Drop sessions that never received a `user_fk` link.
--   2. Swap the FK action from `ON DELETE SET NULL` to `ON DELETE CASCADE`.
--   3. Drop the denormalised `user_id` column + its index.
--   4. Promote `user_fk` to NOT NULL.

DELETE FROM user_auth_sessions WHERE user_fk IS NULL;

ALTER TABLE user_auth_sessions DROP CONSTRAINT fk_user_auth_sessions_user;
DROP INDEX IF EXISTS idx_user_auth_sessions_user_id;
ALTER TABLE user_auth_sessions DROP COLUMN user_id;
ALTER TABLE user_auth_sessions ALTER COLUMN user_fk SET NOT NULL;
ALTER TABLE user_auth_sessions
    ADD CONSTRAINT fk_user_auth_sessions_user
        FOREIGN KEY (user_fk) REFERENCES users(id) ON DELETE CASCADE;
