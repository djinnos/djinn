// Test helper: println reports migration progress under --nocapture.
#![allow(clippy::print_stdout)]
//! Schema application entry point for CI and local dev.
//!
//! This is an `--ignored` test rather than a `[[bin]]` **on purpose**, and the
//! reason is measured, not stylistic. CI's `server-test` cache is warmed by
//! `cargo nextest run --workspace --all-targets --no-run`, which populates
//! test-profile artifacts. A `cargo run --bin` needs the dev-profile path,
//! which that warmer never produces — so a bin recompiled 329 crates from
//! `proc-macro2` (3m49s) while `cargo test -p djinn-db --test ...` against the
//! identical cache in the same run took 27s. Same package, same features; the
//! only difference was the profile.
//!
//! Keep this a test target unless the warm lane starts building dev-profile
//! binaries. `setup_test_template.rs` is the same pattern for the same reason.
//!
//! Required env:
//!   DJINN_MIGRATE_DATABASE_URL        target DSN
//!   DJINN_MIGRATE_OPERATOR_USER_ID    designated operator uuid
//!   DJINN_MIGRATE_OPERATOR_GITHUB_ID  designated operator GitHub id
//!   DJINN_MIGRATE_OPERATOR_LOGIN      designated operator GitHub login

use std::time::Duration;

use djinn_db::migrations::{
    DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
    ensure_postgres_database_exists, run_postgres_migrations,
};
use sqlx::Connection;
use sqlx::postgres::PgConnection;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

/// Wait out a service container that is still accepting connections.
/// `tokio::time::timeout` owns the deadline so this stays clear of the
/// repo-wide `Instant::now` ban (clippy.toml disallowed-methods).
async fn wait_for_database(db_url: &str) {
    let keep_trying = async {
        loop {
            match PgConnection::connect(db_url).await {
                Ok(conn) => {
                    let _ = conn.close().await;
                    return;
                }
                Err(e) => println!("waiting for database ({e})"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(60), keep_trying)
        .await
        .expect("database unreachable after 60s");
}

#[tokio::test]
#[ignore]
async fn apply_migrations() {
    let db_url = required("DJINN_MIGRATE_DATABASE_URL");
    let operator_user_id = required("DJINN_MIGRATE_OPERATOR_USER_ID");
    let github_id: i64 = required("DJINN_MIGRATE_OPERATOR_GITHUB_ID")
        .parse()
        .expect("DJINN_MIGRATE_OPERATOR_GITHUB_ID must be an integer");
    let github_login = required("DJINN_MIGRATE_OPERATOR_LOGIN");

    wait_for_database(&db_url).await;
    ensure_postgres_database_exists(&db_url)
        .await
        .expect("ensure database exists");

    // Same two phases, same order, same owned runner the server uses: the
    // pre-contract boundary provisions the operator, then the full migrator
    // runs with that identity in session scope.
    bootstrap_designated_operator(
        &db_url,
        &DesignatedOperatorBootstrap {
            user_id: operator_user_id.clone(),
            github_id,
            github_login,
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .expect("provision designated operator");
    println!("designated operator provisioned");

    run_postgres_migrations(
        &db_url,
        &MigrationContext {
            designated_operator_user_id: Some(operator_user_id),
        },
    )
    .await
    .expect("apply embedded migrations");
    println!("all migrations applied");
}
