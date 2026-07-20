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
use std::borrow::Cow;
use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions, Connection, Executor};

use crate::error::{DbError, DbResult};

/// Inputs that exist only for a migration session, never a runtime pool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationContext {
    pub designated_operator_user_id: Option<String>,
}

pub const CREATOR_CONTRACT_MIGRATION_VERSION: i64 = 133;

/// Exact caller-selected identity for the fresh-install-only provisioner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesignatedOperatorBootstrap {
    pub user_id: String,
    pub github_id: i64,
    pub github_login: String,
    pub github_name: Option<String>,
    pub github_avatar_url: Option<String>,
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

/// Provision the caller's identity after the pre-contract schema boundary.
/// This never selects an existing user or assigns any role.
pub async fn bootstrap_designated_operator(
    db_url: &str,
    operator: &DesignatedOperatorBootstrap,
) -> DbResult<()> {
    if operator.user_id.trim().is_empty()
        || operator.github_login.trim().is_empty()
        || operator.github_id <= 0
    {
        return Err(DbError::InvalidData(
            "bootstrap designated operator identity must be nonblank and have a positive GitHub id"
                .to_owned(),
        ));
    }
    let options = PgConnectOptions::from_str(db_url)
        .map_err(|e| DbError::InvalidData(format!("invalid postgres url: {e}")))?;
    let mut conn = options.connect().await.map_err(DbError::from)?;
    let result = async {
        let embedded = sqlx::migrate!("./migrations_postgres");
        let pre_contract = sqlx::migrate::Migrator {
            migrations: Cow::Owned(
                embedded
                    .migrations
                    .iter()
                    .filter(|migration| migration.version < CREATOR_CONTRACT_MIGRATION_VERSION)
                    .cloned()
                    .collect(),
            ),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        pre_contract
            .run_direct(&mut conn)
            .await
            .map_err(|e| DbError::InvalidData(e.to_string()))?;
        let has_application_data: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM projects LIMIT 1) \
             OR EXISTS (SELECT 1 FROM epics LIMIT 1) \
             OR EXISTS (SELECT 1 FROM tasks LIMIT 1) \
             OR EXISTS (SELECT 1 FROM sessions LIMIT 1)",
        )
        .fetch_one(&mut conn)
        .await
        .map_err(DbError::from)?;
        if has_application_data {
            return Err(DbError::InvalidData(
                "bootstrap designated operator only accepts a fresh database without application data"
                    .to_owned(),
            ));
        }
        let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&mut conn)
            .await
            .map_err(DbError::from)?;
        if user_count != 0 {
            return Err(DbError::InvalidData(
                "bootstrap designated operator only accepts a fresh database without users"
                    .to_owned(),
            ));
        }
        sqlx::query(
            "INSERT INTO users (id, github_id, github_login, github_name, github_avatar_url) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(operator.user_id.trim())
        .bind(operator.github_id)
        .bind(operator.github_login.trim())
        .bind(operator.github_name.as_deref())
        .bind(operator.github_avatar_url.as_deref())
        .execute(&mut conn)
        .await
        .map_err(DbError::from)?;
        Ok(())
    }
    .await;
    let close_result = conn.close().await.map_err(DbError::from);
    result?;
    close_result
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
