//! The designated-operator GUC belongs only to the explicitly owned migration
//! connection. Independent Postgres sessions prove it cannot leak to a pool.

use djinn_db::migrations::{
    DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
    run_postgres_migrations_on_connection,
};
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

fn base_database_url() -> String {
    std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .unwrap_or_else(|_| {
            "postgres://djinn:VipjO1uAdxAGvNSA6EcJdZMdHAquYeJj@djinn-postgres.djinn.svc.cluster.local:5432/djinn".to_owned()
        })
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

#[tokio::test]
async fn migration_operator_setting_is_connection_local() {
    let prefix = server_prefix(&base_database_url());
    let db_name = format!("djinn_migration_context_{}", uuid::Uuid::now_v7().simple());
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
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&db_url)
        .await
        .expect("open normal runtime pool");
    let before: Option<String> = sqlx::query_scalar(
        "SELECT current_setting('djinn.migration_designated_operator_user_id', true)",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect normal runtime connection before migration");
    assert_eq!(before, None);

    const OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000002";
    bootstrap_designated_operator(
        &db_url,
        &DesignatedOperatorBootstrap {
            user_id: OPERATOR_ID.to_owned(),
            github_id: 9_000_000_002,
            github_login: "migration-context-operator".to_owned(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .expect("provision explicit migration operator");

    let mut migration_conn = PgConnection::connect(&db_url)
        .await
        .expect("open owned migration connection");
    run_postgres_migrations_on_connection(
        &mut migration_conn,
        &MigrationContext {
            designated_operator_user_id: Some(OPERATOR_ID.to_owned()),
        },
    )
    .await
    .expect("run embedded migrations on owned connection");
    let visible: Option<String> = sqlx::query_scalar(
        "SELECT current_setting('djinn.migration_designated_operator_user_id', true)",
    )
    .fetch_one(&mut migration_conn)
    .await
    .expect("inspect owned migration connection");
    assert_eq!(visible.as_deref(), Some(OPERATOR_ID));
    migration_conn
        .close()
        .await
        .expect("close migration connection");

    let after: Option<String> = sqlx::query_scalar(
        "SELECT current_setting('djinn.migration_designated_operator_user_id', true)",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect normal runtime connection after migration");
    assert_eq!(after, None);
    pool.close().await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect postgres admin database");
    admin.execute(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()").as_str()).await.expect("terminate migration test connections");
    admin
        .execute(format!(r#"DROP DATABASE "{db_name}""#).as_str())
        .await
        .expect("drop migration test database");
}
