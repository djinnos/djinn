-- Retire the legacy automation/service-user identity row.
--
-- Attribution columns are nullable by design: deleting a user should leave
-- historical work ownerless rather than dangling. The task/session/epic
-- creator columns already reference users(id) with ON DELETE SET NULL from
-- migration 3. Proposals originally had only a bare nullable author_user_id
-- column, so protect it before removing any users.github_id = 0 row.

-- Clean up any existing dangling proposal authors before enforcing the FK.
UPDATE proposals p
SET author_user_id = NULL
WHERE p.author_user_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1
      FROM users u
      WHERE u.id = p.author_user_id
  );

DO $$
DECLARE
    author_attnum smallint;
    users_id_attnum smallint;
    constraint_name name;
BEGIN
    SELECT attnum
    INTO author_attnum
    FROM pg_attribute
    WHERE attrelid = 'proposals'::regclass
      AND attname = 'author_user_id'
      AND NOT attisdropped;

    SELECT attnum
    INTO users_id_attnum
    FROM pg_attribute
    WHERE attrelid = 'users'::regclass
      AND attname = 'id'
      AND NOT attisdropped;

    -- If a FK already protects proposals.author_user_id with ON DELETE SET NULL,
    -- leave it in place (even if it was created under a different name).
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conrelid = 'proposals'::regclass
          AND confrelid = 'users'::regclass
          AND contype = 'f'
          AND conkey = ARRAY[author_attnum]::smallint[]
          AND confkey = ARRAY[users_id_attnum]::smallint[]
          AND confdeltype = 'n'
    ) THEN
        -- Replace any existing FK on this exact column/reference that would not
        -- null proposal authors when a user row is deleted.
        FOR constraint_name IN
            SELECT conname
            FROM pg_constraint
            WHERE conrelid = 'proposals'::regclass
              AND confrelid = 'users'::regclass
              AND contype = 'f'
              AND conkey = ARRAY[author_attnum]::smallint[]
              AND confkey = ARRAY[users_id_attnum]::smallint[]
        LOOP
            EXECUTE format('ALTER TABLE proposals DROP CONSTRAINT %I', constraint_name);
        END LOOP;

        ALTER TABLE proposals
            ADD CONSTRAINT fk_proposals_author_user
            FOREIGN KEY (author_user_id) REFERENCES users(id) ON DELETE SET NULL;
    END IF;
END $$;

DELETE FROM users WHERE github_id = 0;
