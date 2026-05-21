-- Cut `user_auth_sessions` over to the joined identity model.
--
-- Phase 1 stuffed the GitHub numeric id (as a string) into a denormalised
-- `user_id` column so the web client could echo identity without a join.
-- Phase 2 added `user_fk` pointing at `users.id` (a UUID), and every
-- authenticated path is supposed to read identity through the join.
--
-- The legacy `user_id` column survived as a footgun: a
-- `From<UserAuthSessionRecord> for AuthenticatedUser` impl in the web
-- server pulled `row.user_id` (the GitHub numeric) and fed it into FK
-- columns elsewhere (e.g. `user_settings.user_id`, which references
-- `users.id`). That produced 1452 FK violations on first write the
-- moment a user toggled their auto_approve preference.
--
-- Cut-over:
--   1. Drop sessions that never received a `user_fk` link (legacy Phase 1
--      rows). Those users will be forced to re-OAuth; the callback
--      already writes `user_fk` via `create_with_user_fk`.
--   2. Swap the FK action from `ON DELETE SET NULL` to `ON DELETE CASCADE`
--      — when a user row goes away, their session tokens should die with
--      them rather than orphan as NULL-fk rows that can no longer be
--      authenticated anyway. (Required before we can make `user_fk`
--      NOT NULL: SET NULL forces the column to be nullable.)
--   3. Drop the denormalised `user_id` column + its index.
--   4. Promote `user_fk` to NOT NULL so the type system stops carrying
--      an Option for a column the code can no longer safely treat as
--      missing.

DELETE FROM user_auth_sessions WHERE user_fk IS NULL;

ALTER TABLE user_auth_sessions DROP FOREIGN KEY fk_user_auth_sessions_user;
ALTER TABLE user_auth_sessions DROP INDEX idx_user_auth_sessions_user_id;
ALTER TABLE user_auth_sessions DROP COLUMN user_id;
ALTER TABLE user_auth_sessions MODIFY COLUMN user_fk VARCHAR(36) NOT NULL;
ALTER TABLE user_auth_sessions
    ADD CONSTRAINT fk_user_auth_sessions_user
        FOREIGN KEY (user_fk) REFERENCES users(id) ON DELETE CASCADE;
