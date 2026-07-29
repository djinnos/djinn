//! Migration 162 — durable build-pod permit schema contract.
//!
//! Exercises the exact PostgreSQL schema introduced for build-pod permits with
//! the repository's isolated-database migration harness. In particular, a Job
//! becoming terminal is deliberately not represented as a release: its
//! `job_created` permit remains active until fenced release metadata is set.

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

fn base_database_url() -> String {
    djinn_db::test_database_base_url()
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

#[tokio::test]
async fn migration_162_enforces_build_pod_permit_contract() {
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
