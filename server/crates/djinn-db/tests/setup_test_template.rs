// Test helper: println used for migration progress output with --nocapture.
#![allow(clippy::print_stdout)]
//! One-shot setup helper for `djinn_test_template` using the owned runner.

#[tokio::test]
#[ignore]
async fn setup_test_template() {
    use djinn_db::migrations::{
        DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
        run_postgres_migrations,
    };
    use sqlx::Connection;
    use sqlx::postgres::PgConnection;

    const TEMPLATE_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000001";
    let base = djinn_db::test_database_base_url();
    let server_prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base);
    let admin_url = format!("{server_prefix}/postgres");
    let mut conn = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    let _ = sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 'djinn_test_template' AND pid <> pg_backend_pid()")
        .execute(&mut conn).await;
    let _ = sqlx::query("DROP DATABASE IF EXISTS djinn_test_template")
        .execute(&mut conn)
        .await;
    sqlx::query("CREATE DATABASE djinn_test_template")
        .execute(&mut conn)
        .await
        .expect("create template");
    drop(conn);

    let template_url = format!("{server_prefix}/djinn_test_template");
    bootstrap_designated_operator(
        &template_url,
        &DesignatedOperatorBootstrap {
            user_id: TEMPLATE_OPERATOR_ID.to_owned(),
            github_id: 9_000_000_001,
            github_login: "djinn-test-template-operator".to_owned(),
            github_name: Some("Djinn test template operator".to_owned()),
            github_avatar_url: None,
        },
    )
    .await
    .expect("provision reserved template operator");
    run_postgres_migrations(
        &template_url,
        &MigrationContext {
            designated_operator_user_id: Some(TEMPLATE_OPERATOR_ID.to_owned()),
        },
    )
    .await
    .expect("apply embedded migrations");

    let mut conn = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect admin");
    sqlx::query(
        "UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut conn)
    .await
    .expect("mark template");
    println!("djinn_test_template ready at {template_url}");
}
