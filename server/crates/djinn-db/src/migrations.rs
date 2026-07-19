//! Schema migrations for djinn-db.
//!
//! The project uses [`sqlx::migrate!`] as the single source of truth for
//! the Postgres backend. Migrations live under `migrations_postgres/` at
//! the crate root and are embedded into the binary at compile time.
//!
//! Adding a migration: create the next `{N}_{slug}.sql` under that
//! directory. NEVER edit an applied migration — sqlx stores a checksum in
//! `_sqlx_migrations` and will refuse to start if the on-disk content
//! diverges. Tests enforce this (`tests/migrations_immutable.rs`).
use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions, Connection, Executor};

use crate::error::{DbError, DbResult};

/// Inputs that exist only for a migration session, never a runtime pool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationContext {
    pub designated_operator_user_id: Option<String>,
}

/// Run the embedded Postgres migrator on one owned connection.
///
/// The setting is deliberately session scoped and this connection is closed
/// before returning, so it cannot reach normal application query traffic.
pub async fn run_postgres_migrations(db_url: &str, context: &MigrationContext) -> DbResult<()> {
    let options = PgConnectOptions::from_str(db_url)
        .map_err(|e| DbError::InvalidData(format!("invalid postgres url: {e}")))?;
    let mut conn = options.connect().await.map_err(DbError::from)?;
    let result = run_postgres_migrations_on_connection(&mut conn, context).await;
    let close_result = conn.close().await.map_err(DbError::from);
    result?;
    close_result
}

/// Exact-connection variant used by the isolated test-template bootstrap and
/// migration fixtures. It is not suitable for runtime pool connections.
pub async fn run_postgres_migrations_on_connection(
    conn: &mut PgConnection,
    context: &MigrationContext,
) -> DbResult<()> {
    sqlx::query("SET statement_timeout = 0")
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    if let Some(operator_id) = context.designated_operator_user_id.as_deref() {
        let operator_id = operator_id.trim();
        if operator_id.is_empty() {
            return Err(DbError::InvalidData(
                "migration designated operator user id must not be blank".to_owned(),
            ));
        }
        sqlx::query("SELECT set_config('djinn.migration_designated_operator_user_id', $1, false)")
            .bind(operator_id)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
    }
    sqlx::migrate!("./migrations_postgres")
        .run_direct(&mut *conn)
        .await
        .map_err(|e: sqlx::migrate::MigrateError| DbError::InvalidData(e.to_string()))
}

/// Run only migrations before the creator-contract boundary for the two
/// creating it via a side connection to the `postgres` maintenance database
/// if necessary.
///
/// sqlx will not `CREATE DATABASE` for us — the pool connects with the
/// database selected, so this has to run first.
pub async fn ensure_postgres_database_exists(db_url: &str) -> DbResult<()> {
    let Some(database) = extract_postgres_database_name(db_url) else {
        return Ok(());
    };
    if !is_safe_database_identifier(&database) {
        return Err(DbError::InvalidData(format!(
            "unsafe postgres database name `{database}`; only [A-Za-z0-9_] allowed"
        )));
    }

    // Side connection against the `postgres` maintenance database. Postgres
    // does not support `CREATE DATABASE IF NOT EXISTS` so we probe first
    // and only issue the CREATE when missing.
    let opts = PgConnectOptions::from_str(db_url)
        .map_err(|e| DbError::InvalidData(format!("invalid postgres url: {e}")))?
        .database("postgres");
    let mut conn = opts.connect().await.map_err(DbError::from)?;
    let exists: Option<i32> = sqlx::query_scalar!(
        r#"SELECT 1 AS "exists!" FROM pg_database WHERE datname = $1"#,
        database
    )
    .fetch_optional(&mut conn)
    .await
    .map_err(DbError::from)?;
    if exists.is_none() {
        let stmt = format!(r#"CREATE DATABASE "{database}""#);
        conn.execute(stmt.as_str()).await.map_err(DbError::from)?;
    }
    conn.close().await.map_err(DbError::from)?;
    Ok(())
}

fn extract_postgres_database_name(db_url: &str) -> Option<String> {
    let trimmed = db_url.trim();
    let without_scheme = trimmed
        .strip_prefix("postgres://")
        .or_else(|| trimmed.strip_prefix("postgresql://"))?;
    let after_host = without_scheme.rsplit('@').next().unwrap_or(without_scheme);
    let (_host, path) = after_host.split_once('/')?;
    let name = path.split('?').next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

fn is_safe_database_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
