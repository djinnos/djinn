//! Migration 162 — durable build-pod permit schema contract.
//!
//! Exercises the exact PostgreSQL schema introduced for build-pod permits with
//! the repository's isolated-database migration harness. In particular, a Job
//! becoming terminal is deliberately not represented as a release: its
//! `job_created` permit remains active until fenced release metadata is set.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 162;
const MIGRATION_FILE: &str = "162_build_pod_permits.sql";
const RESIZE_MIGRATION_FILE: &str = "163_build_pod_resize_identity.sql";
const MIGRATION_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000162";
const CREATOR_CONTRACT_VERSION: u64 = 142;

fn base_database_url() -> String {
    djinn_db::test_database_base_url()
}

#[tokio::test]
async fn migration_163_upgrades_released_162_permits_without_job_uids() {
    with_temp_database("upgrade_resize_identity", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url).await.unwrap();
        apply_prior_migrations(&mut conn).await;
        apply_permit_migration(&mut conn).await;
        let pool = PgPoolOptions::new().max_connections(1).connect(&db_url).await.unwrap();
        seed_task_runs(&pool, MIGRATION_OPERATOR_ID, &["released"]).await;
        sqlx::query("INSERT INTO build_pod_permits (task_run_id, fencing_token, state, released_at, released_fencing_token, release_reason) VALUES ('released', 9001, 'released', now(), 9001, 'before_job')")
            .execute(&pool).await.unwrap();
        pool.close().await;
        apply_resize_migration(&mut conn).await;
        let pool = PgPoolOptions::new().max_connections(1).connect(&db_url).await.unwrap();
        let row: (Option<String>, String) = sqlx::query_as("SELECT job_uid, state FROM build_pod_permits WHERE task_run_id = 'released'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(row, (None, "released".into()));
        pool.close().await;
    }).await;
}

async fn apply_resize_migration(conn: &mut PgConnection) {
    let sql = std::fs::read_to_string(migrations_dir().join(RESIZE_MIGRATION_FILE))
        .expect("read resize migration sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply resize migration after migration 162");
}

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .map(|entry| {
            let path = entry.expect("migration dir entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let version = name
                .split_once('_')
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| {
            path.extension().and_then(|extension| extension.to_str()) == Some("sql")
        })
        .collect();
    entries.sort_by(|(left_version, left_path), (right_version, right_path)| {
        left_version.cmp(right_version).then_with(|| {
            left_path
                .file_name()
                .unwrap_or_default()
                .cmp(right_path.file_name().unwrap_or_default())
        })
    });
    entries
}

async fn seed_migration_operator(conn: &mut PgConnection) {
    conn.execute(
        format!(
            "INSERT INTO users (id, github_id, github_login) VALUES \
             ('{MIGRATION_OPERATOR_ID}', 9000000162, 'permit-migration-operator') ON CONFLICT DO NOTHING"
        )
        .as_str(),
    )
    .await
    .expect("seed designated migration operator");
}

/// Apply the complete pre-162 schema without using the embedded migrator, so
/// representative live rows can exist before the additive permit migration.
async fn apply_prior_migrations(conn: &mut PgConnection) {
    conn.execute(
        format!(
            "SELECT set_config('djinn.migration_designated_operator_user_id', '{MIGRATION_OPERATOR_ID}', false)"
        )
        .as_str(),
    )
    .await
    .expect("set designated operator GUC");

    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        if version == 0 {
            continue;
        }
        if version == CREATOR_CONTRACT_VERSION {
            seed_migration_operator(conn).await;
        }
        let sql = std::fs::read_to_string(&path).expect("read prior migration sql");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply migration {} failed: {error}", path.display()));
    }
}

async fn apply_permit_migration(conn: &mut PgConnection) {
    let sql = std::fs::read_to_string(migrations_dir().join(MIGRATION_FILE))
        .expect("read permit migration sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply permit migration after prior migrations");
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let db_name = format!(
        "djinn_migration_{}_{}",
        suffix,
        uuid::Uuid::now_v7().simple()
    );
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect postgres admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create migration test database");
    drop(admin);

    let db_url = format!("{prefix}/{db_name}");
    let result = f(db_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{db_name}""#).as_str())
        .await
        .expect("drop migration test database");

    result
}

async fn seed_task_runs(pool: &sqlx::PgPool, creator_id: &str, run_ids: &[&str]) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('permit-project', 'permit-project', 'djinnos', 'permit-contract')",
    )
    .execute(pool)
    .await
    .expect("seed permit project");

    for (index, run_id) in run_ids.iter().enumerate() {
        let task_id = format!("permit-task-{index}");
        let short_id = format!("permit-{index}");
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, labels, \
              acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ($1, 'permit-project', $2, 'title', 'description', 'design', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $3)",
        )
        .bind(&task_id)
        .bind(&short_id)
        .bind(creator_id)
        .execute(pool)
        .await
        .expect("seed permit task");

        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
             VALUES ($1, 'permit-project', $2, 'manual', 'completed')",
        )
        .bind(run_id)
        .bind(&task_id)
        .execute(pool)
        .await
        .expect("seed permit task run");
    }
}

async fn active_permit_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM build_pod_permits WHERE state <> 'released'")
        .fetch_one(pool)
        .await
        .expect("count active build-pod permits")
}

async fn seed_preexisting_build_lease_and_admission_data(pool: &sqlx::PgPool) {
    sqlx::query(
        "UPDATE build_lease_caps \
         SET cap = 7, updated_at = '2025-01-02T03:04:05Z' \
         WHERE singleton",
    )
    .execute(pool)
    .await
    .expect("seed pre-existing build lease cap");
    sqlx::query(
        "INSERT INTO build_leases \
         (consumer_kind, consumer_id, immutable_identity, fencing_token, state, \
          bound_pod_uid, weight, created_at, updated_at, granted_at) \
         VALUES ('task_dispatch', 'preexisting-task', 'preexisting-identity', 741, 'active', \
                 'preexisting-pod', 2, '2025-01-02T03:04:05Z', '2025-01-02T03:04:06Z', \
                 '2025-01-02T03:04:06Z')",
    )
    .execute(pool)
    .await
    .expect("seed pre-existing build lease");
    sqlx::query(
        "INSERT INTO admission_journal \
         (domain, work_id, generation, workload_kind, state, creator_server_epoch, \
          object_name, object_uid, created_at, updated_at) \
         VALUES ('invocation_build', 'preexisting-work', 3, 'invocation', 'live', \
                 'preexisting-epoch', 'preexisting-job', 'preexisting-job-uid', \
                 '2025-01-02T03:04:05Z', '2025-01-02T03:04:06Z')",
    )
    .execute(pool)
    .await
    .expect("seed pre-existing admission journal row");
    sqlx::query(
        "UPDATE admission_handoff \
         SET phase = 'invocation_primary', epoch = 7, v0_mode = 'disabled', \
             v1_mode = 'enforce', cap = 7, updated_at = '2025-01-02T03:04:07Z' \
         WHERE name = 'build'",
    )
    .execute(pool)
    .await
    .expect("seed pre-existing admission handoff");
}

async fn existing_build_lease_and_admission_snapshot(pool: &sqlx::PgPool) -> Vec<String> {
    let lease: String = sqlx::query_scalar(
        "SELECT concat_ws('|', consumer_kind, consumer_id, immutable_identity, fencing_token, \
                state, bound_pod_uid, weight, created_at::text, updated_at::text, granted_at::text) \
         FROM build_leases WHERE consumer_id = 'preexisting-task'",
    )
    .fetch_one(pool)
    .await
    .expect("read pre-existing build lease");
    let lease_cap: String = sqlx::query_scalar(
        "SELECT concat_ws('|', cap, updated_at::text) FROM build_lease_caps WHERE singleton",
    )
    .fetch_one(pool)
    .await
    .expect("read pre-existing build lease cap");
    let journal: String = sqlx::query_scalar(
        "SELECT concat_ws('|', domain, work_id, generation, workload_kind, state, \
                creator_server_epoch, object_name, object_uid, created_at::text, updated_at::text) \
         FROM admission_journal \
         WHERE domain = 'invocation_build' AND work_id = 'preexisting-work' AND generation = 3",
    )
    .fetch_one(pool)
    .await
    .expect("read pre-existing admission journal row");
    let handoff: String = sqlx::query_scalar(
        "SELECT concat_ws('|', name, phase, epoch, v0_mode, v1_mode, cap, updated_at::text) \
         FROM admission_handoff WHERE name = 'build'",
    )
    .fetch_one(pool)
    .await
    .expect("read pre-existing admission handoff");
    vec![lease, lease_cap, journal, handoff]
}

#[tokio::test]
async fn migration_162_embedded_fresh_database_enforces_build_pod_permit_contract() {
    with_temp_database("build_pod_permits", |db_url| async move {
        let creator_id =
            djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");
        let run_ids = [
            "permit-run-acquired",
            "permit-run-job-created",
            "permit-run-released",
            "permit-run-invalid",
            "permit-run-distinct",
        ];
        seed_task_runs(&pool, &creator_id, &run_ids).await;

        // Migration initialization creates exactly the one lockable global pool.
        let pool_keys: Vec<String> =
            sqlx::query_scalar("SELECT pool_key FROM build_pod_permit_pools ORDER BY pool_key")
                .fetch_all(&pool)
                .await
                .expect("load initialized permit pools");
        assert_eq!(pool_keys, ["global"]);
        assert!(
            sqlx::query("INSERT INTO build_pod_permit_pools (pool_key) VALUES ('other')")
                .execute(&pool)
                .await
                .is_err(),
            "the pool CHECK must reject identities other than global"
        );
        assert!(
            sqlx::query("INSERT INTO build_pod_permit_pools (pool_key) VALUES ('global')")
                .execute(&pool)
                .await
                .is_err(),
            "the pool primary key must prevent a second global row"
        );

        // Acquiring before Job creation permits a NULL Job UID.
        sqlx::query(
            "INSERT INTO build_pod_permits (task_run_id, fencing_token, state) \
             VALUES ('permit-run-acquired', 1001, 'acquired')",
        )
        .execute(&pool)
        .await
        .expect("insert acquired permit without Job UID");
        let acquired_uid: Option<String> = sqlx::query_scalar(
            "SELECT job_uid FROM build_pod_permits WHERE task_run_id = 'permit-run-acquired'",
        )
        .fetch_one(&pool)
        .await
        .expect("read acquired permit Job UID");
        assert_eq!(acquired_uid, None);

        // A present Job UID transitions the permit to job_created. This row
        // models a terminal-but-still-present Job: state is intentionally not
        // released, so it must remain part of active admission accounting.
        sqlx::query(
            "INSERT INTO build_pod_permits (task_run_id, fencing_token, state, job_uid) \
             VALUES ('permit-run-job-created', 1002, 'job_created', 'terminal-job-uid')",
        )
        .execute(&pool)
        .await
        .expect("insert job-created permit");

        // A correctly fenced explicit release is valid, including without a
        // Job UID. Release metadata, rather than terminality, excludes it.
        sqlx::query(
            "INSERT INTO build_pod_permits \
             (task_run_id, fencing_token, state, released_at, released_fencing_token, release_reason) \
             VALUES ('permit-run-released', 1003, 'released', now(), 1003, 'job_terminal')",
        )
        .execute(&pool)
        .await
        .expect("insert explicitly released permit");

        // One lifecycle per task run is enforced, while another task run can
        // independently acquire a permit.
        assert!(
            sqlx::query(
                "INSERT INTO build_pod_permits (task_run_id, fencing_token, state) \
                 VALUES ('permit-run-acquired', 1004, 'acquired')",
            )
            .execute(&pool)
            .await
            .is_err(),
            "a second permit for the same task_run_id must be rejected"
        );
        sqlx::query(
            "INSERT INTO build_pod_permits (task_run_id, fencing_token, state) \
             VALUES ('permit-run-distinct', 1005, 'acquired')",
        )
        .execute(&pool)
        .await
        .expect("distinct task run can acquire a permit");

        // State, Job UID, and release-fencing combinations are schema-level
        // invariants rather than repository conventions.
        for sql in [
            "INSERT INTO build_pod_permits (task_run_id, fencing_token, state) \
             VALUES ('permit-run-invalid', 1101, 'job_created')",
            "INSERT INTO build_pod_permits (task_run_id, fencing_token, state, job_uid) \
             VALUES ('permit-run-invalid', 1102, 'acquired', 'premature-job-uid')",
            "INSERT INTO build_pod_permits \
             (task_run_id, fencing_token, state, released_at, released_fencing_token, release_reason) \
             VALUES ('permit-run-invalid', 1103, 'released', now(), 9999, 'wrong-fence')",
            "UPDATE build_pod_permits SET released_at = now() \
             WHERE task_run_id = 'permit-run-acquired'",
            "UPDATE build_pod_permits SET fencing_token = 2001 \
             WHERE task_run_id = 'permit-run-job-created'",
            "UPDATE build_pod_permits SET job_uid = 'replacement-job-uid' \
             WHERE task_run_id = 'permit-run-job-created'",
        ] {
            assert!(
                sqlx::query(sql).execute(&pool).await.is_err(),
                "permit schema should reject invalid contract SQL: {sql}"
            );
        }

        // The downstream active-count query is authoritative: all and only
        // non-released rows count. The terminal-but-present Job is active here.
        assert_eq!(active_permit_count(&pool).await, 3);
        sqlx::query(
            "UPDATE build_pod_permits \
             SET state = 'released', released_at = now(), \
                 released_fencing_token = fencing_token, release_reason = 'job_terminal' \
             WHERE task_run_id = 'permit-run-job-created'",
        )
        .execute(&pool)
        .await
        .expect("explicitly release terminal-but-present Job permit");
        assert_eq!(active_permit_count(&pool).await, 2);

        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_162_upgrades_prior_schema_without_rewriting_existing_admission_data() {
    with_temp_database("upgrade_build_pod_permits", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect prior schema pool");
        seed_preexisting_build_lease_and_admission_data(&pool).await;
        let before = existing_build_lease_and_admission_snapshot(&pool).await;
        pool.close().await;

        apply_permit_migration(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect upgraded database");
        assert_eq!(
            existing_build_lease_and_admission_snapshot(&pool).await,
            before,
            "the additive permit migration must not rewrite existing build-lease or admission rows"
        );

        let permit_table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public.build_pod_permits')::text")
                .fetch_one(&pool)
                .await
                .expect("inspect permit table");
        let pool_table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public.build_pod_permit_pools')::text")
                .fetch_one(&pool)
                .await
                .expect("inspect permit pool table");
        assert_eq!(permit_table.as_deref(), Some("build_pod_permits"));
        assert_eq!(pool_table.as_deref(), Some("build_pod_permit_pools"));

        let initialized_pool: (String, String) = sqlx::query_as(
            "SELECT pool_key, created_at::text FROM build_pod_permit_pools WHERE pool_key = 'global'",
        )
        .fetch_one(&pool)
        .await
        .expect("read initialized singleton pool");

        // Replay the exact migration initialization statement. Its conflict
        // action must neither add another row nor replace the existing row.
        sqlx::query(
            "INSERT INTO build_pod_permit_pools (pool_key) VALUES ('global') \
             ON CONFLICT (pool_key) DO NOTHING",
        )
        .execute(&pool)
        .await
        .expect("replay permit pool initialization");
        let pools: Vec<(String, String)> =
            sqlx::query_as("SELECT pool_key, created_at::text FROM build_pod_permit_pools ORDER BY pool_key")
                .fetch_all(&pool)
                .await
                .expect("read pools after initialization replay");
        assert_eq!(pools, vec![initialized_pool]);

        pool.close().await;
    })
    .await;
}
