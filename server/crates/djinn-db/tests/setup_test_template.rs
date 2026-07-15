// Test helper: println used for migration progress output with --nocapture.
#![allow(clippy::print_stdout)]
//! One-shot setup helper: build the `djinn_test_template` database on the
//! configured DJINN_TEST_DATABASE_URL (default = the cluster Postgres) and
//! apply every `migrations_postgres/*.sql` file. Run with:
//!   cargo test -p djinn-db --test setup_test_template -- --ignored --nocapture
//!
//! Idempotent: drops the template first if it exists, then re-creates it.
//! Marks the result as a TEMPLATE so `Database::open_in_memory()` can clone
//! it via `CREATE DATABASE x TEMPLATE djinn_test_template`.

#[tokio::test]
#[ignore]
async fn setup_test_template() {
    use sqlx::Connection;
    use sqlx::postgres::PgConnection;

    let base = std::env::var("DJINN_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://djinn:VipjO1uAdxAGvNSA6EcJdZMdHAquYeJj@djinn-postgres.djinn.svc.cluster.local:5432/djinn".to_owned());
    // Strip the trailing `/<db>` to get the server prefix.
    let server_prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base)
        .to_owned();
    let admin_url = format!("{server_prefix}/postgres");

    let mut conn = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = 'djinn_test_template' AND pid <> pg_backend_pid()",
    )
    .execute(&mut conn)
    .await;
    sqlx::query(
        "UPDATE pg_database SET datistemplate = FALSE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut conn)
    .await
    .expect("unmark template");
    sqlx::query("DROP DATABASE IF EXISTS djinn_test_template")
        .execute(&mut conn)
        .await
        .expect("drop template");
    sqlx::query("CREATE DATABASE djinn_test_template")
        .execute(&mut conn)
        .await
        .expect("create template");
    drop(conn);

    let template_url = format!("{server_prefix}/djinn_test_template");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&template_url)
        .await
        .expect("connect template");
    sqlx::migrate!("./migrations_postgres")
        .run(&pool)
        .await
        .expect("apply migrations");
    pool.close().await;

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
