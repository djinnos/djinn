-- Least-privilege platform-database role for task-run Pods.
--
-- ============================================================================
-- WHY THIS EXISTS
-- ============================================================================
--
-- On 2026-08-04 the production platform database was found with migration
-- 182's DDL applied and no `_sqlx_migrations` ledger row for it, which
-- crash-looped the migrate init container on the v0.7.41 deploy. sqlx 0.8.6
-- cannot produce that state: `execute_migration` runs the migration SQL and
-- the ledger INSERT in one transaction. So the DDL was applied out-of-band, by
-- something holding a DDL-capable connection to the platform database.
--
-- Who is unprovable — production runs with `log_statement = none`. This role
-- makes the question moot rather than answering it: a task-run Pod that cannot
-- issue DDL and does not own any relation cannot reach that state at all,
-- whatever runs inside it.
--
-- The companion change (djinn-agent-worker `platform_dsn`, and the deny-set in
-- `djinn_cgroup_launcher::env`) removes the DSN from the environment of every
-- process the agent spawns. This role is the second, independent control: it
-- bounds what the DSN can do even when something legitimately holds it.
--
-- ============================================================================
-- WHAT THE POD ACTUALLY NEEDS (enumerated from the code, not assumed)
-- ============================================================================
--
-- `bootstrap_warm_database()` (djinn-agent-worker/src/main.rs) opens this DSN
-- and calls `verify_and_mark_initialized()`. Traced from there:
--
--   * The worker NEVER migrates. `verify_and_mark_initialized` reads
--     `information_schema.tables` and `SELECT version, success FROM
--     _sqlx_migrations`, then marks the handle initialized so no later
--     repository call can trigger a lazy migrator. `sqlx::migrate!(...)` is
--     expanded only to enumerate compiled-in versions in memory; `.run()` is
--     never called on this path. => the role needs SELECT on
--     `_sqlx_migrations` and nothing else on it. It must NOT be able to write
--     that table: an INSERT there is how a partially-applied migration would
--     be papered over.
--
--   * Session GUCs on every pooled connection: `statement_timeout`,
--     `lock_timeout`, `idle_in_transaction_session_timeout`. Plain session
--     SETs; no privilege needed.
--
--   * The durable invocation-lease authority reads
--     `SELECT ... FROM admission_handoff WHERE name = $1`. Read only — every
--     write path on that table (seed/set_mode/upsert/delete, and the
--     `SELECT ... FOR UPDATE` arms) belongs to the coordinator, not the pod.
--
--   * The rest is ordinary board DML: `activity_log`, `task_runs`, `tasks`,
--     `task_attempts`, `task_arbitrations`, `blockers`, `sessions`,
--     `session_messages`, `session_compaction_boundaries`, `notes` and the
--     `note_*` tables, `retrieval_traces`, `extension_load_diagnostics`,
--     `projects`, `agents`, `epics`, `proposals`, the `evidence_*` and
--     `typed_evidence_*` families -- plus everything else the agent's own MCP
--     tool surface can reach, because `AgentContext.db` is the unnarrowed
--     platform handle and it is wired into `DjinnMcpServer`.
--
--   * `pg_advisory_xact_lock` on the note-write path. Advisory locks need no
--     GRANT.
--
--   * NO sequence privileges. Verified against the migrated schema: the only
--     serial/identity columns in `public` are `model_turn_pools.id`,
--     `build_pod_permits.fencing_token`, `repo_graph_generation.publish_seq`
--     and `build_leases.enqueue_sequence`, none of which a task-run Pod
--     inserts into. `nextval('build_lease_fencing_token_seq')` and
--     `nextval('board_health_mismatch_scan_leader_epoch_seq')` are
--     coordinator-side. So the role gets no USAGE on any sequence, and an
--     attempt to insert into a host-owned ledger fails on the sequence as well
--     as on the table.
--
--   * NO row-level security is defined anywhere in the schema, so a non-owner
--     role is not silently subject to policies the owner bypassed.
--
-- ============================================================================
-- SCOPE OF STAGE 1 (this file) vs STAGE 2 (deferred)
-- ============================================================================
--
-- STAGE 1, applied by this script, removes the two capabilities that produced
-- the incident and the two categories that have no business in a Pod:
--
--   * no DDL of any kind, and no ownership of any relation;
--   * no write access to `_sqlx_migrations`;
--   * no access at all to the credential and auth tables;
--   * DML on the remaining board tables, which is what the agent's tool
--     surface uses today.
--
-- STAGE 2 -- narrowing table-by-table to the deterministic set enumerated
-- above -- is NOT in this file, and deliberately so. `AgentContext.db` hands
-- the whole platform `Database` to the MCP tool dispatcher
-- (`djinn-agent/src/context.rs`, `fn db()` / `to_mcp_state()`), which
-- reconstructs `DjinnMcpServer` inside the Pod. A per-table grant derived from
-- the deterministic path would break `memory_*`, `task_*`, `epic_*`,
-- `proposal_*` and the evidence tools the moment an agent used one. The
-- prerequisite is routing those helpers through `SupervisorServices` RPC so
-- the Pod needs no board `Database` at all; see the PR body.
--
-- ============================================================================
-- HOW TO APPLY  (idempotent; safe against an existing database)
-- ============================================================================
--
--   1. Create the login role and its password OUT OF BAND -- never in this
--      repository:
--
--        CREATE ROLE djinn_task_run_pod LOGIN PASSWORD '<generated>'
--          NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
--
--   2. Run this script as the database owner (the role that owns the tables,
--      i.e. the one the migrate Job connects as):
--
--        psql "$DJINN_DATABASE_URL" -v ON_ERROR_STOP=1 \
--          -f deploy/roles/djinn_task_run.sql
--
--   3. GRANT djinn_task_run TO djinn_task_run_pod;
--
--   4. Point ONLY the task-run Pod's `DJINN_DATABASE_URL` at
--      `djinn_task_run_pod`. djinn-server, the coordinator and the migrate Job
--      keep the owner DSN -- the migrate Job in particular MUST stay the owner
--      or migrations stop working.
--
--   5. Re-run this script after any migration that adds a table, so the new
--      table is covered and any new secret table is revoked. Step 6's
--      verification will tell you if you forgot.
--
--   6. Verify (expects zero rows from each):
--
--        SELECT has_schema_privilege('djinn_task_run','public','CREATE')
--          WHERE has_schema_privilege('djinn_task_run','public','CREATE');
--
--        SELECT c.relname FROM pg_class c JOIN pg_namespace n
--          ON n.oid = c.relnamespace
--         WHERE n.nspname = 'public'
--           AND pg_get_userbyid(c.relowner) = 'djinn_task_run';
--
--        SELECT table_name, privilege_type
--          FROM information_schema.role_table_grants
--         WHERE grantee = 'djinn_task_run'
--           AND (table_name = '_sqlx_migrations' AND privilege_type <> 'SELECT'
--                OR table_name IN ('credentials','custom_providers',
--                                  'mcp_access_tokens','oauth_clients',
--                                  'oauth_authorization_codes',
--                                  'user_auth_sessions'));
--
-- ============================================================================

\set ON_ERROR_STOP on

BEGIN;

-- ---------------------------------------------------------------------------
-- The role itself. NOLOGIN: it is a privilege bundle, not an identity. The
-- identity is a separate login role the operator creates with a password that
-- never enters this repository, and which is granted this one.
--
-- Every attribute here is a capability this role must not have. NOCREATEDB and
-- NOCREATEROLE are not decoration: they are the difference between "cannot
-- issue DDL against these tables" and "cannot build somewhere else to issue it
-- from".
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'djinn_task_run') THEN
        CREATE ROLE djinn_task_run NOLOGIN;
    END IF;
END
$$;

ALTER ROLE djinn_task_run
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;

-- ---------------------------------------------------------------------------
-- Schema access: USAGE, never CREATE.
--
-- `REVOKE CREATE ON SCHEMA public FROM PUBLIC` is the load-bearing line for
-- databases initialised before PostgreSQL 15, where `public` was writable by
-- every role by default. Without it the role could `CREATE TABLE` in `public`
-- no matter what is or is not granted below, and the whole point of this file
-- would be decorative.
-- ---------------------------------------------------------------------------
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
REVOKE ALL ON SCHEMA public FROM djinn_task_run;
GRANT USAGE ON SCHEMA public TO djinn_task_run;

-- No database-level CREATE either (that is CREATE SCHEMA).
DO $$
BEGIN
    EXECUTE format('REVOKE CREATE ON DATABASE %I FROM PUBLIC', current_database());
    EXECUTE format('REVOKE CREATE ON DATABASE %I FROM djinn_task_run', current_database());
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO djinn_task_run', current_database());
END
$$;

-- ---------------------------------------------------------------------------
-- Start from nothing, every run. This is what makes the script idempotent
-- against a database whose grants have drifted, and what makes step 5
-- (re-running after a migration) converge rather than accumulate.
-- ---------------------------------------------------------------------------
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM djinn_task_run;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM djinn_task_run;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM djinn_task_run;

-- ---------------------------------------------------------------------------
-- Board DML on everything except the tables named below.
--
-- TRUNCATE and REFERENCES are excluded on purpose. TRUNCATE is a destructive
-- verb no application path uses, and REFERENCES lets a role create a foreign
-- key -- which is DDL, and would also pin the referenced rows.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    target text;
    -- Tables the Pod must never read or write.
    --
    -- The credential and auth surface. `AgentContext.db` reaches
    -- `CredentialRepository` and the OAuth/token repositories through the same
    -- handle, even though the deterministic worker path never calls them
    -- (`provider_override.is_some()` short-circuits credential resolution).
    -- The Pod already receives the one credential it needs as a mounted
    -- Secret, so nothing legitimate is lost.
    secret_tables constant text[] := ARRAY[
        'credentials',
        'custom_providers',
        'mcp_access_tokens',
        'oauth_authorization_codes',
        'oauth_clients',
        'user_auth_sessions'
    ];
    -- Tables the Pod may read but must never write.
    --
    -- `_sqlx_migrations` is the object the incident corrupted: the boot-time
    -- schema verification reads it, and nothing in a Pod has any business
    -- writing it. `admission_handoff` is the durable invocation-lease
    -- authority -- the Pod projects it into a lift decision; only the
    -- coordinator arms it.
    read_only_tables constant text[] := ARRAY[
        '_sqlx_migrations',
        'admission_handoff'
    ];
BEGIN
    FOR target IN
        SELECT tablename FROM pg_tables WHERE schemaname = 'public'
    LOOP
        IF target = ANY (secret_tables) THEN
            CONTINUE;
        ELSIF target = ANY (read_only_tables) THEN
            EXECUTE format('GRANT SELECT ON public.%I TO djinn_task_run', target);
        ELSE
            EXECUTE format(
                'GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO djinn_task_run',
                target
            );
        END IF;
    END LOOP;
END
$$;

-- ---------------------------------------------------------------------------
-- Future tables.
--
-- Without this, the next migration that adds a table takes the Pod down: the
-- table exists, the code writes it, and the role has no privilege on it. The
-- default is deliberately the permissive arm -- a new SECRET table is the case
-- the operator must handle, and step 5 plus step 6's verification is how.
--
-- `FOR ROLE` is derived from whoever owns the tables today rather than
-- hard-coded, because default privileges attach to the CREATING role and the
-- migrate Job's identity is a deployment detail, not a constant.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    owner_role text;
BEGIN
    SELECT pg_get_userbyid(c.relowner)
      INTO owner_role
      FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public'
       AND c.relkind = 'r'
     GROUP BY pg_get_userbyid(c.relowner)
     ORDER BY count(*) DESC
     LIMIT 1;

    IF owner_role IS NULL THEN
        RAISE EXCEPTION
            'no tables in schema public: run this after the migrations, not before';
    END IF;

    EXECUTE format(
        'ALTER DEFAULT PRIVILEGES FOR ROLE %I IN SCHEMA public '
        'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO djinn_task_run',
        owner_role
    );
    -- No default sequence or function privileges: see the header note on why
    -- the Pod needs neither.
    EXECUTE format(
        'ALTER DEFAULT PRIVILEGES FOR ROLE %I IN SCHEMA public '
        'REVOKE ALL ON SEQUENCES FROM djinn_task_run',
        owner_role
    );
END
$$;

-- ---------------------------------------------------------------------------
-- Assert the properties this file claims, inside the same transaction that
-- established them. A grant script that cannot fail is a grant script nobody
-- can trust; these three checks are the reason the incident is now
-- structurally impossible for this role rather than merely unlikely.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    offender text;
BEGIN
    IF has_schema_privilege('djinn_task_run', 'public', 'CREATE') THEN
        RAISE EXCEPTION 'djinn_task_run has CREATE on schema public; it must not';
    END IF;

    SELECT string_agg(c.relname, ', ')
      INTO offender
      FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public'
       AND pg_get_userbyid(c.relowner) = 'djinn_task_run';
    IF offender IS NOT NULL THEN
        RAISE EXCEPTION 'djinn_task_run owns relations (%); it must own none', offender;
    END IF;

    SELECT string_agg(format('%s:%s', table_name, privilege_type), ', ')
      INTO offender
      FROM information_schema.role_table_grants
     WHERE grantee = 'djinn_task_run'
       AND table_schema = 'public'
       AND ((table_name = '_sqlx_migrations' AND privilege_type <> 'SELECT')
            OR table_name IN ('credentials', 'custom_providers', 'mcp_access_tokens',
                              'oauth_authorization_codes', 'oauth_clients',
                              'user_auth_sessions'));
    IF offender IS NOT NULL THEN
        RAISE EXCEPTION 'djinn_task_run holds forbidden grants (%)', offender;
    END IF;

    IF EXISTS (
        SELECT 1 FROM pg_roles
         WHERE rolname = 'djinn_task_run'
           AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolbypassrls)
    ) THEN
        RAISE EXCEPTION 'djinn_task_run carries a role attribute it must not';
    END IF;
END
$$;

COMMIT;
