//! Schema migrations for djinn-db.
//!
//! The project uses [`sqlx::migrate!`] as the single source of truth for
//! both SQLite and MySQL/Dolt backends. Migrations live under
//! `migrations_sqlite/` and `migrations_mysql/` at the crate root and are
//! embedded into the binary at compile time.
//!
//! Adding a migration: create the next `V{N}__{slug}.sql` under the correct
//! directory. NEVER edit an applied migration — sqlx stores a checksum in
//! `_sqlx_migrations` and will refuse to start if the on-disk content
//! diverges. Tests enforce this (`tests/migrations_immutable.rs`).
use std::str::FromStr;

use sqlx::mysql::MySqlConnectOptions;
use sqlx::{ConnectOptions, Connection, Executor};

use crate::error::{DbError, DbResult};

/// Ensure a MySQL/Dolt database named in `db_url` exists on the server,
/// creating it via a side connection without a default schema if necessary.
///
/// sqlx will not `CREATE DATABASE` for us — the pool connects with the
/// database selected, so this has to run first.
pub async fn ensure_mysql_database_exists(db_url: &str) -> DbResult<()> {
    let Some(database) = extract_mysql_database_name(db_url) else {
        return Ok(());
    };
    if !is_safe_database_identifier(&database) {
        return Err(DbError::InvalidData(format!(
            "unsafe mysql database name `{database}`; only [A-Za-z0-9_] allowed"
        )));
    }

    let opts = MySqlConnectOptions::from_str(db_url)
        .map_err(|e| DbError::InvalidData(format!("invalid mysql url: {e}")))?
        .database("");
    let mut conn = opts.connect().await.map_err(DbError::from)?;
    let stmt = format!("CREATE DATABASE IF NOT EXISTS `{database}`");
    conn.execute(stmt.as_str()).await.map_err(DbError::from)?;
    conn.close().await.map_err(DbError::from)?;
    Ok(())
}

fn extract_mysql_database_name(db_url: &str) -> Option<String> {
    let trimmed = db_url.trim();
    let without_scheme = trimmed.strip_prefix("mysql://")?;
    let after_host = without_scheme.rsplit('@').next().unwrap_or(without_scheme);
    let mut parts = after_host.splitn(2, '/');
    let _host = parts.next()?;
    let path = parts.next()?;
    let name = path.split('?').next()?.trim();
    if name.is_empty() { None } else { Some(name.to_owned()) }
}

fn is_safe_database_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}
