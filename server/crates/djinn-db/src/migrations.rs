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

use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions, Connection, Executor};

use crate::error::{DbError, DbResult};

/// Ensure a Postgres database named in `db_url` exists on the server,
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
    let exists: Option<i32> =
        sqlx::query_scalar!(r#"SELECT 1 AS "exists!" FROM pg_database WHERE datname = $1"#, database)
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
