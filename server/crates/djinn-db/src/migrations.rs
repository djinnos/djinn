use std::path::Path;
use std::str::FromStr;

use sqlx::mysql::MySqlConnectOptions;
use sqlx::{ConnectOptions, Connection, Executor, MySqlPool};

use crate::error::{DbError, DbResult};

mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations");
}

const SQLITE_SCHEMA_SNAPSHOT: &str = include_str!("../schema.sql");
const MYSQL_SCHEMA_SNAPSHOT: &str = include_str!("../sql/mysql_schema.sql");
const MYSQL_NOTES_FULLTEXT_PROTOTYPE: &str =
    include_str!("../sql/mysql_notes_fulltext_prototype.sql");
const MYSQL_MIGRATION_V3_USER_AUTH_SESSIONS: &str =
    include_str!("../sql/migrations_mysql/V3__user_auth_sessions.sql");

/// Ordered list of incremental ALTER migrations applied after the initial
/// snapshot. Version 0 is reserved for the snapshot itself. To add a new
/// migration:
///
///   1. Create `sql/migrations_mysql/V{N}__{slug}.sql` with the next N.
///   2. Append `(N, "{slug}", include_str!("../sql/migrations_mysql/V{N}__{slug}.sql"))`
///      to this slice, keeping versions strictly increasing.
///   3. Write deterministic ALTERs — the runner fails hard on any error for
///      incremental migrations (unlike the idempotent initial snapshot).
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "example_schema_evolution",
        include_str!("../sql/migrations_mysql/V1__example_schema_evolution.sql"),
    ),
    (
        2,
        "projects_github_columns",
        include_str!("../sql/migrations_mysql/V2__projects_github_columns.sql"),
    ),
    (
        4,
        "projects_installation_id",
        include_str!("../sql/migrations_mysql/V4__projects_installation_id.sql"),
    ),
];

/// Run migrations using refinery's built-in rusqlite runner.
///
/// Refinery handles checksum validation (rejects modified migrations) and
/// ordering enforcement (rejects out-of-order versions) automatically.
pub fn run(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = rusqlite::Connection::open(path)?;
    embedded::migrations::runner().run(&mut conn)?;
    Ok(())
}

/// Return the canonical SQLite schema snapshot used by the current runtime.
pub fn sqlite_schema_snapshot() -> &'static str {
    SQLITE_SCHEMA_SNAPSHOT
}

/// Return the staged Dolt/MySQL schema snapshot for ADR-055 note/task/session state.
///
/// This artifact is intentionally separate from the embedded SQLite refinery migrations so the
/// existing SQLite runtime remains selectable while the MySQL/Dolt migration path is explicit.
pub fn mysql_schema_snapshot() -> &'static str {
    MYSQL_SCHEMA_SNAPSHOT
}

/// Ensure a MySQL/Dolt database named in `db_url` exists on the server,
/// creating it via a side connection without a default schema if necessary.
///
/// This is required because sqlx pools connect lazily with the database
/// selected — if the database does not yet exist the first pool acquire fails
/// with "database not found". The side connection bypasses the database
/// selection until CREATE DATABASE succeeds.
pub async fn ensure_mysql_database_exists(db_url: &str) -> DbResult<()> {
    let Some(database) = extract_mysql_database_name(db_url) else {
        return Ok(());
    };
    if !is_safe_database_identifier(&database) {
        return Err(DbError::InvalidData(format!(
            "unsafe mysql database name `{database}`; only [A-Za-z0-9_] allowed"
        )));
    }

    // Parse full options then clear the database selection so CREATE DATABASE
    // can run before the target schema exists.
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

/// Apply the MySQL/Dolt schema against `pool`, bringing it up to the latest
/// known version.
///
/// Two-phase:
///
/// 1. **Bootstrap** — ensure the `djinn_schema_migrations` marker table
///    exists, transparently upgrading any legacy `djinn_schema_version`
///    marker (single-row presence check) to the new (version, name,
///    applied_at) layout by inserting version 0 = "initial_snapshot".
/// 2. **Snapshot (version 0)** — if no rows are present, apply
///    [`mysql_schema_snapshot`] idempotently (tolerating "already exists"
///    errors so repeat runs are safe) and record version 0.
/// 3. **Incremental (versions >= 1)** — for each entry in [`MIGRATIONS`]
///    whose version is not yet recorded, apply it and record it. Incremental
///    migrations are expected to be deterministic — any statement failure
///    aborts the boot with the migration left unrecorded. MySQL/Dolt commit
///    DDL implicitly, so partial failure is possible; recovery is
///    developer-side (inspect, hand-fix, add a follow-up migration).
pub async fn ensure_mysql_schema(pool: &MySqlPool) -> DbResult<()> {
    ensure_migrations_table(pool).await?;
    migrate_legacy_schema_version(pool).await?;
    apply_initial_snapshot_if_needed(pool).await?;
    apply_incremental_migrations(pool).await?;
    Ok(())
}

async fn ensure_migrations_table(pool: &MySqlPool) -> DbResult<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS djinn_schema_migrations (\
            version BIGINT NOT NULL PRIMARY KEY, \
            name VARCHAR(191) NOT NULL, \
            applied_at VARCHAR(64) NOT NULL \
                DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))\
        )",
    )
    .execute(pool)
    .await
    .map_err(DbError::from)?;
    Ok(())
}

/// If the deployed DB still has the old `djinn_schema_version` marker and
/// no rows in the new `djinn_schema_migrations` table, seed version 0
/// ("initial_snapshot") so the incremental runner treats the existing
/// database as already bootstrapped.
async fn migrate_legacy_schema_version(pool: &MySqlPool) -> DbResult<()> {
    let new_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM djinn_schema_migrations")
            .fetch_one(pool)
            .await
            .map_err(DbError::from)?;
    if new_count.0 > 0 {
        return Ok(());
    }

    let legacy_exists: Option<(String,)> = sqlx::query_as(
        "SELECT TABLE_NAME FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'djinn_schema_version' \
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(DbError::from)?;
    if legacy_exists.is_none() {
        return Ok(());
    }

    let legacy_row: Option<(String,)> =
        sqlx::query_as("SELECT version FROM djinn_schema_version LIMIT 1")
            .fetch_optional(pool)
            .await
            .map_err(DbError::from)?;
    if legacy_row.is_none() {
        return Ok(());
    }

    tracing::info!(
        "upgrading legacy djinn_schema_version marker to djinn_schema_migrations"
    );
    sqlx::query(
        "INSERT INTO djinn_schema_migrations (version, name) VALUES (0, 'initial_snapshot')",
    )
    .execute(pool)
    .await
    .map_err(DbError::from)?;
    Ok(())
}

async fn apply_initial_snapshot_if_needed(pool: &MySqlPool) -> DbResult<()> {
    let snapshot_applied: Option<(i64,)> = sqlx::query_as(
        "SELECT version FROM djinn_schema_migrations WHERE version = 0",
    )
    .fetch_optional(pool)
    .await
    .map_err(DbError::from)?;
    if snapshot_applied.is_some() {
        return Ok(());
    }

    for stmt in split_sql_statements(MYSQL_SCHEMA_SNAPSHOT) {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Err(err) = sqlx::query(trimmed).execute(pool).await {
            // The initial snapshot is idempotent by construction: MySQL/Dolt
            // lack CREATE INDEX IF NOT EXISTS, so we tolerate benign
            // "already exists" errors here (and ONLY here — incremental
            // migrations get no such tolerance).
            let msg = err.to_string().to_ascii_lowercase();
            if msg.contains("duplicate key name")
                || msg.contains("already exists")
                || (msg.contains("table") && msg.contains("exists"))
            {
                tracing::debug!(
                    stmt = %preview(trimmed),
                    "skipping already-applied snapshot statement"
                );
                continue;
            }
            return Err(DbError::InvalidData(format!(
                "failed to apply mysql schema snapshot statement: {err}; statement preview: {}",
                preview(trimmed)
            )));
        }
    }

    sqlx::query(
        "INSERT INTO djinn_schema_migrations (version, name) VALUES (0, 'initial_snapshot')",
    )
    .execute(pool)
    .await
    .map_err(DbError::from)?;
    Ok(())
}

async fn apply_incremental_migrations(pool: &MySqlPool) -> DbResult<()> {
    // Sanity check: versions strictly increasing, all >= 1.
    let mut last = 0_i64;
    for (version, name, _) in MIGRATIONS {
        if *version < 1 {
            return Err(DbError::InvalidData(format!(
                "invalid migration version {version} for `{name}`: must be >= 1 \
                 (version 0 is reserved for the initial snapshot)"
            )));
        }
        if *version <= last {
            return Err(DbError::InvalidData(format!(
                "migrations out of order at version {version} (`{name}`): \
                 versions must be strictly increasing"
            )));
        }
        last = *version;
    }

    let applied: Vec<(i64,)> = sqlx::query_as(
        "SELECT version FROM djinn_schema_migrations WHERE version > 0",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)?;
    let applied: std::collections::HashSet<i64> =
        applied.into_iter().map(|(v,)| v).collect();

    for (version, name, sql) in MIGRATIONS {
        if applied.contains(version) {
            continue;
        }
        tracing::info!(version, name, "applying mysql schema migration");
        for stmt in split_sql_statements(sql) {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Err(err) = sqlx::query(trimmed).execute(pool).await {
                tracing::error!(
                    version,
                    name,
                    stmt = %preview(trimmed),
                    error = %err,
                    "incremental migration statement failed; database may be \
                     in a partial state — recover by hand and follow up with a \
                     new migration"
                );
                return Err(DbError::InvalidData(format!(
                    "failed to apply mysql migration V{version}__{name}: {err}; \
                     statement preview: {}",
                    preview(trimmed)
                )));
            }
        }
        sqlx::query(
            "INSERT INTO djinn_schema_migrations (version, name) VALUES (?, ?)",
        )
        .bind(version)
        .bind(*name)
        .execute(pool)
        .await
        .map_err(DbError::from)?;
    }
    Ok(())
}

fn preview(stmt: &str) -> String {
    let first_line = stmt.lines().next().unwrap_or("").trim();
    let max_chars = 120;
    if first_line.chars().count() > max_chars {
        let truncated: String = first_line.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        first_line.to_owned()
    }
}

/// Split a SQL script into individual statements on top-level `;` boundaries,
/// ignoring semicolons inside line comments and string literals. The mysql
/// schema snapshot does not use stored-procedure syntax, so this keeps the
/// parser simple.
fn split_sql_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut prev = '\0';

    for ch in sql.chars() {
        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
            }
            current.push(ch);
            prev = ch;
            continue;
        }
        if in_block_comment {
            if prev == '*' && ch == '/' {
                in_block_comment = false;
            }
            current.push(ch);
            prev = ch;
            continue;
        }
        if in_single_quote {
            if ch == '\'' && prev != '\\' {
                in_single_quote = false;
            }
            current.push(ch);
            prev = ch;
            continue;
        }
        if in_double_quote {
            if ch == '"' && prev != '\\' {
                in_double_quote = false;
            }
            current.push(ch);
            prev = ch;
            continue;
        }

        match ch {
            '-' if prev == '-' => {
                in_line_comment = true;
                current.push(ch);
            }
            '*' if prev == '/' => {
                in_block_comment = true;
                current.push(ch);
            }
            '\'' => {
                in_single_quote = true;
                current.push(ch);
            }
            '"' => {
                in_double_quote = true;
                current.push(ch);
            }
            ';' => {
                if !current.trim().is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
        prev = ch;
    }

    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Return the reference MySQL FULLTEXT query prototype paired with the staged schema snapshot.
pub fn mysql_notes_fulltext_prototype() -> &'static str {
    MYSQL_NOTES_FULLTEXT_PROTOTYPE
}

/// Return the staged MySQL migrations for the web-client GitHub OAuth session table.
///
/// Registered as a staging artifact alongside `mysql_schema_snapshot` so the MySQL/Dolt
/// cutover path carries the same `user_auth_sessions` table the SQLite runtime creates
/// via refinery. The tuple is `(version, name, sql)`.
pub fn staged_mysql_migrations() -> &'static [(i64, &'static str, &'static str)] {
    &[(
        3,
        "user_auth_sessions",
        MYSQL_MIGRATION_V3_USER_AUTH_SESSIONS,
    )]
}

/// Return the embedded migration list (version, name, checksum) for testing.
#[cfg(test)]
pub(crate) fn embedded_checksums() -> Vec<(i64, String, u64)> {
    embedded::migrations::runner()
        .get_migrations()
        .iter()
        .map(|m| (m.version(), m.name().to_string(), m.checksum()))
        .collect()
}

#[cfg(test)]
pub(crate) fn run_until(
    path: &Path,
    migration_name_exclusive: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = rusqlite::Connection::open(path)?;
    let runner = embedded::migrations::runner();
    let migrations = runner.get_migrations();
    let stop_version = migrations
        .iter()
        .find(|migration| migration.name() == migration_name_exclusive)
        .map(|migration| migration.version() - 1)
        .unwrap_or_else(|| {
            migrations
                .last()
                .map(|migration| migration.version())
                .unwrap_or(0)
        });

    runner
        .set_target(refinery::Target::Version(stop_version))
        .run(&mut conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{mysql_notes_fulltext_prototype, mysql_schema_snapshot, sqlite_schema_snapshot};

    #[test]
    fn sqlite_snapshot_retains_sqlite_specific_search_structures() {
        let schema = sqlite_schema_snapshot();
        assert!(schema.contains("CREATE VIRTUAL TABLE notes_fts USING fts5"));
        assert!(schema.contains("CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN"));
    }

    #[test]
    fn mysql_snapshot_replaces_fts_shadow_table_with_fulltext_index() {
        let schema = mysql_schema_snapshot();
        assert!(schema.contains("ALTER TABLE notes ADD FULLTEXT KEY notes_ft"));
        assert!(!schema.contains("CREATE VIRTUAL TABLE notes_fts USING fts5"));
        assert!(!schema.contains("CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN"));
        assert!(!schema.contains("vec0("));
    }

    #[test]
    fn mysql_artifacts_document_clear_parallel_cutover_path() {
        let schema = mysql_schema_snapshot();
        let prototype = mysql_notes_fulltext_prototype();

        assert!(schema.contains("CREATE TABLE tasks"));
        assert!(schema.contains("CREATE TABLE notes"));
        assert!(schema.contains("CREATE TABLE sessions"));
        assert!(prototype.contains("MATCH(n.title, n.content, n.tags) AGAINST"));
    }

    #[test]
    fn mysql_snapshot_includes_user_auth_sessions() {
        let schema = mysql_schema_snapshot();
        assert!(schema.contains("CREATE TABLE user_auth_sessions"));
    }

    #[test]
    fn staged_mysql_migrations_exposes_user_auth_sessions_v3() {
        let migrations = super::staged_mysql_migrations();
        let (version, _, sql) = migrations
            .iter()
            .find(|(_, n, _)| *n == "user_auth_sessions")
            .expect("V3 migration registered");
        assert_eq!(*version, 3);
        assert!(sql.contains("CREATE TABLE user_auth_sessions"));
    }
}
