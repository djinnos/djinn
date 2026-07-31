//! Migration 163 execution-generation and task-scoped liveness evidence contract.

use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{Connection, Executor};

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = djinn_db::test_database_base_url();
    let prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base)
        .trim_end_matches('/');
    let db_name = format!("djinn_migration_{suffix}_{}", uuid::Uuid::now_v7().simple());
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

async fn seed_task(pool: &PgPool, creator_id: &str) -> String {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('generation-project', 'generation-project', 'djinnos', 'generation-contract')",
    )
    .execute(pool)
    .await
    .unwrap();
    let task_id = "generation-task".to_owned();
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
         VALUES ($1, 'generation-project', 'gen163', 'title', 'description', 'design', \
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $2)",
    )
    .bind(&task_id)
    .bind(creator_id)
    .execute(pool)
    .await
    .unwrap();
    task_id
}

#[tokio::test]
async fn migration_163_adds_generation_and_task_owned_reconciliation_evidence() {
    with_temp_database("execution_generation", |db_url| async move {
        let creator_id =
            djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");
        let task_id = seed_task(&pool, &creator_id).await;

        let generation: i64 =
            sqlx::query_scalar("SELECT execution_generation FROM tasks WHERE id = $1")
                .bind(&task_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(generation, 0);

        for (index, outcome) in [
            "success",
            "crash",
            "timeout",
            "dead_reclaimed",
            "protocol_violation",
            "kill_noop",
            "slow_extended",
            "terminated",
            "desync_reconciled",
            "genuinely_absent",
            "task_not_found",
            "teardown_failed",
            "settlement_failed",
            "reconciliation_incomplete",
            "audit_failed",
        ]
        .into_iter()
        .enumerate()
        {
            sqlx::query(
                "INSERT INTO liveness_evidence (id, session_id, task_id, verdict, outcome_kind) \
                 VALUES ($1, NULL, $2, 'dead', $3)",
            )
            .bind(format!("generation-evidence-{index}"))
            .bind(&task_id)
            .bind(outcome)
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("outcome {outcome} should be accepted: {error}"));
        }

        let no_owner = sqlx::query(
            "INSERT INTO liveness_evidence (id, session_id, task_id, verdict) \
             VALUES ('generation-no-owner', NULL, NULL, 'dead')",
        )
        .execute(&pool)
        .await;
        assert!(
            no_owner.is_err(),
            "evidence must have a session or task owner"
        );

        let invalid_session = sqlx::query(
            "INSERT INTO liveness_evidence (id, session_id, task_id, verdict) \
             VALUES ('generation-invalid-session', 'missing-session', $1, 'dead')",
        )
        .bind(&task_id)
        .execute(&pool)
        .await;
        assert!(
            invalid_session.is_err(),
            "non-null session IDs retain their FK"
        );
    })
    .await;
}
