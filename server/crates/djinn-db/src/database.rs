use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::OnceCell;

use crate::error::{DbError, DbResult};
use crate::migrations;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseBackendKind {
    Postgres,
}

impl DatabaseBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum DatabaseConnectConfig {
    Postgres(PostgresDatabaseConfig),
}

impl DatabaseConnectConfig {
    pub fn backend_kind(&self) -> DatabaseBackendKind {
        match self {
            Self::Postgres(_) => DatabaseBackendKind::Postgres,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostgresDatabaseConfig {
    pub url: String,
}

impl PostgresDatabaseConfig {
    pub fn display_backend(&self) -> &'static str {
        "postgres"
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseBootstrapInfo {
    pub backend_kind: DatabaseBackendKind,
    pub backend_label: String,
    pub target: String,
    pub readonly: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteSearchBackend {
    SqliteFts5,
    PostgresTsvector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteVectorBackend {
    SqliteVec,
    External,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseBackendCapabilities {
    pub backend_kind: DatabaseBackendKind,
    pub backend_label: String,
    pub lexical_search: NoteSearchBackend,
    pub note_vector_backend: NoteVectorBackend,
    pub supports_sqlite_pragmas: bool,
    pub supports_sqlite_vec: bool,
    pub supports_readonly_connection_mode: bool,
}

#[derive(Clone, Debug)]
pub struct Database {
    pool: PgPool,
    readonly: bool,
    bootstrap: DatabaseBootstrapInfo,
    capabilities: DatabaseBackendCapabilities,
    initialized: Arc<OnceCell<()>>,
    test_branch: Option<TestDbInit>,
}

/// Postgres template-clone test isolation: per-test `djinn_test_<uuid>`
/// database cloned from `djinn_test_template` via `CREATE DATABASE … TEMPLATE`.
#[derive(Clone, Debug)]
struct TestDbInit {
    server_prefix: String,
    test_db: String,
}

impl Database {
    /// Open a database using an explicit backend selection seam.
    pub fn open_with_config(config: DatabaseConnectConfig) -> DbResult<Self> {
        match config {
            // 32 max connections gives reads + concurrent writers headroom
            // for the workspace's expected concurrency (coordinator,
            // mirror-fetcher, KB watcher, MCP tools, PR poller).
            DatabaseConnectConfig::Postgres(pg) => Self::open_postgres(&pg, 32),
        }
    }

    /// Open an isolated test database.
    ///
    /// Uses the Postgres template-clone approach: take the base URL from
    /// `DJINN_TEST_DATABASE_URL` (default
    /// `postgres://postgres:postgres@127.0.0.1:5433/postgres`), strip any
    /// trailing `/<db>` segment, then construct an admin connection against
    /// `<base>/postgres`, `CREATE DATABASE djinn_test_<uuid> TEMPLATE
    /// djinn_test_template`, and connect the per-test pool to it.
    ///
    /// Postgres copies indexes, FK constraints, generated columns, and
    /// defaults cleanly via `CREATE DATABASE … TEMPLATE`, so no
    /// `SHOW CREATE TABLE` replay is needed.
    ///
    /// Each test gets a UUIDv7-named database — no collisions under
    /// `cargo nextest` parallelism.
    pub fn open_in_memory() -> DbResult<Self> {
        let base = std::env::var("DJINN_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:5433/postgres".to_owned());
        let server_prefix = strip_server_prefix(&base);
        let test_db = format!("djinn_test_{}", uuid::Uuid::now_v7().simple());
        let url = format!("{server_prefix}/{test_db}");

        Self::open_postgres_inner(
            &PostgresDatabaseConfig { url },
            4,
            Some(TestDbInit {
                server_prefix,
                test_db,
            }),
        )
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn postgres_pool(&self) -> Option<&PgPool> {
        Some(&self.pool)
    }

    pub fn bootstrap_info(&self) -> &DatabaseBootstrapInfo {
        &self.bootstrap
    }

    pub fn backend_capabilities(&self) -> &DatabaseBackendCapabilities {
        &self.capabilities
    }

    pub fn backend_kind(&self) -> DatabaseBackendKind {
        self.bootstrap.backend_kind
    }

    fn open_postgres(config: &PostgresDatabaseConfig, max_connections: u32) -> DbResult<Self> {
        Self::open_postgres_inner(config, max_connections, None)
    }

    fn open_postgres_inner(
        config: &PostgresDatabaseConfig,
        max_connections: u32,
        test_branch: Option<TestDbInit>,
    ) -> DbResult<Self> {
        let mut opts = PgConnectOptions::from_str(&config.url)?;
        // For the Postgres test path, disable sqlx's prepared-statement cache.
        // Parallel tests rapidly CREATE / DROP databases with the same table
        // names (`djinn_test_<uuid>` instances cloned from the same template);
        // we want each per-test pool to fresh-prepare so prior cached plan
        // metadata can't cross-contaminate.
        if test_branch.is_some() {
            opts = opts.statement_cache_capacity(0);
        }
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            // Postgres lock acquisition is single-digit-ms in normal operation;
            // a 15s acquire ceiling surfaces real pool exhaustion as a hard
            // signal rather than masking it behind a 120s wait.
            .acquire_timeout(std::time::Duration::from_secs(15))
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    // Bound how long any single statement can run before the
                    // server kills it.
                    sqlx::query!("SET statement_timeout = '30s'")
                        .execute(&mut *conn)
                        .await?;
                    // Bound how long any explicit lock acquisition can wait.
                    sqlx::query!("SET lock_timeout = '10s'")
                        .execute(&mut *conn)
                        .await?;
                    // Reap connections that started a transaction and walked
                    // away; matches MySQL's wait_timeout role.
                    sqlx::query!("SET idle_in_transaction_session_timeout = '60s'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(opts);

        let backend_label = config.display_backend().to_owned();
        Ok(Self {
            pool,
            readonly: false,
            bootstrap: DatabaseBootstrapInfo {
                backend_kind: DatabaseBackendKind::Postgres,
                backend_label: backend_label.clone(),
                target: config.url.clone(),
                readonly: false,
            },
            capabilities: DatabaseBackendCapabilities {
                backend_kind: DatabaseBackendKind::Postgres,
                backend_label,
                lexical_search: NoteSearchBackend::PostgresTsvector,
                note_vector_backend: NoteVectorBackend::External,
                supports_sqlite_pragmas: false,
                supports_sqlite_vec: false,
                supports_readonly_connection_mode: false,
            },
            initialized: Arc::new(OnceCell::new()),
            test_branch,
        })
    }

    /// Return `true` if a table with the given name exists in the current
    /// database schema.
    ///
    /// Exists as a deliberate test fixture: contract tests verify that the
    /// in-memory test database has its migrations applied. Production code
    /// should not need to probe the schema at runtime.
    pub async fn table_exists(&self, table_name: &str) -> DbResult<bool> {
        self.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM information_schema.tables
             WHERE table_schema = current_schema() AND table_name = $1"#,
            table_name,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn sqlite_vec_status(&self) -> DbResult<SqliteVecStatus> {
        Ok(SqliteVecStatus {
            available: false,
            version: None,
            detail: Some("sqlite-vec retired with the Postgres migration".to_owned()),
        })
    }

    /// Read-only sanity check: confirm every migration baked into this
    /// binary has already been applied successfully on the live DB.
    ///
    /// **Does not** call `sqlx::migrate!(...).run(...)` — that path takes
    /// the migrator's advisory lock. Migrations now live in the Helm
    /// `pre-upgrade` Job (`djinn-server --migrate-only`). App pods just
    /// verify the schema is current and refuse to start otherwise — a
    /// much faster, lock-free boot path.
    ///
    /// Returns `Err` when:
    ///   * The DB doesn't have a `_sqlx_migrations` table at all (fresh
    ///     DB — the Job must run first).
    ///   * Any migration version known to the binary is missing from the
    ///     DB or is recorded as `success = false` (Job didn't finish).
    pub async fn verify_schema_is_current(&self) -> DbResult<()> {
        if self.readonly {
            return Ok(());
        }

        let migrator = sqlx::migrate!("./migrations_postgres");
        let expected_versions: Vec<i64> = migrator.migrations.iter().map(|m| m.version).collect();
        if expected_versions.is_empty() {
            return Ok(());
        }

        // Detect the "no migrations table at all" case up front so we can
        // give the operator a clear error message instead of a generic
        // "_sqlx_migrations not found" SQL exception.
        let migrations_table_exists: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64"
             FROM information_schema.tables
             WHERE table_schema = current_schema()
               AND table_name   = '_sqlx_migrations'"#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from)?;
        if migrations_table_exists == 0 {
            return Err(DbError::InvalidData(
                "_sqlx_migrations table missing — the migration Job has not run yet. \
                 Run `djinn-server --migrate-only` against this database before booting \
                 the app pod (the Helm `pre-upgrade` hook normally handles this)."
                    .to_owned(),
            ));
        }

        let applied: Vec<(i64, bool)> =
            sqlx::query_as("SELECT version, success FROM _sqlx_migrations")
                .fetch_all(&self.pool)
                .await
                .map_err(DbError::from)?;
        let mut applied_ok = std::collections::HashSet::new();
        for (version, success) in &applied {
            if *success {
                applied_ok.insert(*version);
            }
        }

        let mut missing: Vec<i64> = expected_versions
            .iter()
            .copied()
            .filter(|v| !applied_ok.contains(v))
            .collect();
        missing.sort_unstable();
        if !missing.is_empty() {
            return Err(DbError::InvalidData(format!(
                "schema is behind the binary — missing migrations: {missing:?}. \
                 The Helm `pre-upgrade` Job should have applied these via \
                 `djinn-server --migrate-only` before this pod started.",
            )));
        }

        Ok(())
    }

    pub async fn ensure_initialized(&self) -> DbResult<()> {
        if self.readonly {
            return Ok(());
        }

        let pool = self.pool.clone();
        let url = self.bootstrap.target.clone();
        let test_branch = self.test_branch.clone();
        self.initialized
            .get_or_try_init(|| async move {
                match test_branch {
                    Some(TestDbInit {
                        server_prefix,
                        test_db,
                    }) => {
                        // Clone the pre-built `djinn_test_template` database
                        // into the per-test database. The template Job
                        // (`make test-db-postgres-template`) must have run
                        // already — if it hasn't, the clone fails with a
                        // clear "template database does not exist" error
                        // and the operator knows what to do.
                        clone_postgres_test_template(&server_prefix, &test_db).await?;
                    }
                    None => {
                        migrations::ensure_postgres_database_exists(&url).await?;
                        sqlx::migrate!("./migrations_postgres")
                            .run(&pool)
                            .await
                            .map_err(|e: sqlx::migrate::MigrateError| {
                                DbError::InvalidData(e.to_string())
                            })?;
                    }
                }
                Ok::<(), DbError>(())
            })
            .await?;
        Ok(())
    }
}

fn strip_server_prefix(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    match trimmed.strip_prefix("postgres://") {
        Some(after_scheme) => match after_scheme.find('/') {
            Some(slash) => format!("postgres://{}", &after_scheme[..slash]),
            None => format!("postgres://{after_scheme}"),
        },
        None => match trimmed.strip_prefix("postgresql://") {
            Some(after_scheme) => match after_scheme.find('/') {
                Some(slash) => format!("postgresql://{}", &after_scheme[..slash]),
                None => format!("postgresql://{after_scheme}"),
            },
            None => trimmed.to_owned(),
        },
    }
}

/// Clone `djinn_test_template` into a freshly-created per-test database via
/// `CREATE DATABASE … TEMPLATE`.
///
/// Postgres copies indexes, FK constraints, generated columns, defaults, and
/// any rows in the template — no `SHOW CREATE TABLE` replay needed. The
/// template must have no concurrent client connections at clone time, so we
/// terminate any other backends connected to it first.
async fn clone_postgres_test_template(server_prefix: &str, test_db: &str) -> DbResult<()> {
    if !is_safe_database_identifier(test_db) {
        return Err(DbError::InvalidData(format!(
            "unsafe postgres database name `{test_db}`; only [A-Za-z0-9_] allowed",
        )));
    }

    use sqlx::ConnectOptions;
    // Connect to the maintenance `postgres` database (admin connection
    // cannot be open against the template DB at clone time).
    let admin_url = format!("{server_prefix}/postgres");
    let opts = sqlx::postgres::PgConnectOptions::from_str(&admin_url).map_err(DbError::from)?;
    let mut conn = opts.connect().await.map_err(DbError::from)?;

    // Boot any stragglers off the template. Without this, parallel tests
    // racing on the template see `ERROR: source database "djinn_test_template"
    // is being accessed by other users`.
    // Non-macro: this is a side-effecting `SELECT pg_terminate_backend(...)`
    // executed for effect only. The `query!` macro types it as a row-returning
    // query (no `.execute()`), so we keep the plain form here.
    sqlx::query(
        "SELECT pg_terminate_backend(pid) \
           FROM pg_stat_activity \
          WHERE datname = 'djinn_test_template' \
            AND pid <> pg_backend_pid()",
    )
    .execute(&mut conn)
    .await
    .map_err(DbError::from)?;

    let stmt = format!(r#"CREATE DATABASE "{test_db}" TEMPLATE djinn_test_template"#);
    sqlx::query(stmt.as_str())
        .execute(&mut conn)
        .await
        .map_err(DbError::from)?;

    drop(conn);
    Ok(())
}

fn is_safe_database_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Workspace-local tempdir helper retained for tests that still stage
/// on-disk fixtures (project filesystems, worktree mirrors, etc.).
pub(crate) fn test_tempdir() -> DbResult<tempfile::TempDir> {
    let base = workspace_test_tmp_dir()?;
    tempfile::Builder::new()
        .prefix("djinn-test-")
        .tempdir_in(base)
        .map_err(|e| DbError::InvalidData(e.to_string()))
}

fn workspace_test_tmp_dir() -> DbResult<std::path::PathBuf> {
    use std::path::PathBuf;

    // Prefer an explicit override for constrained CI/dev environments.
    if let Some(override_dir) = std::env::var_os("DJINN_TEST_TMPDIR") {
        let path = PathBuf::from(override_dir);
        std::fs::create_dir_all(&path).map_err(|e| DbError::InvalidData(e.to_string()))?;
        return Ok(path);
    }

    // Otherwise place test tempdirs under the workspace root when discoverable.
    if let Some(base) = workspace_root_from_current_dir() {
        let candidate = base.join("target").join("test-tmp");
        std::fs::create_dir_all(&candidate).map_err(|e| DbError::InvalidData(e.to_string()))?;
        return Ok(candidate);
    }

    // Final fallback: root under the current crate's target directory.
    let current_dir = std::env::current_dir().map_err(|e| DbError::InvalidData(e.to_string()))?;
    let fallback = current_dir.join("target").join("test-tmp");
    std::fs::create_dir_all(&fallback).map_err(|e| DbError::InvalidData(e.to_string()))?;
    Ok(fallback)
}

fn workspace_root_from_current_dir() -> Option<std::path::PathBuf> {
    let mut current = std::env::current_dir().ok()?;

    loop {
        let candidate = current.join("Cargo.lock");
        if candidate.exists() {
            return Some(current);
        }

        if !current.pop() {
            return None;
        }
    }
}

/// Legacy vector-extension status. The sqlite-vec backend is retired; this
/// type is retained for call-site compatibility and always reports
/// `available = false` now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqliteVecStatus {
    pub available: bool,
    pub version: Option<String>,
    pub detail: Option<String>,
}

#[cfg(test)]
pub(crate) fn set_sqlite_vec_disabled_for_tests(_disabled: bool) {
    // No-op: sqlite-vec is retired. Retained as a shim so embedding tests
    // compile pending their rewrite against Qdrant.
}

/// Default database path: `~/.djinn/djinn.db`.
///
/// Retained for CLI-plumbing backward compatibility even though the
/// SQLite backend is retired; external callers may still consult this
/// when they have a historical path to reason about.
pub fn default_db_path() -> std::path::PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".djinn")
        .join("djinn.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_backend_selection_metadata_is_shaped_as_expected() {
        let db = Database::open_with_config(DatabaseConnectConfig::Postgres(
            PostgresDatabaseConfig {
                url: "postgres://postgres:postgres@127.0.0.1:5433/djinn".to_owned(),
            },
        ))
        .expect("postgres backend should construct a concrete runtime path");

        assert_eq!(db.backend_kind(), DatabaseBackendKind::Postgres);
        assert_eq!(db.bootstrap_info().backend_label, "postgres");
        assert_eq!(
            db.backend_capabilities().lexical_search,
            NoteSearchBackend::PostgresTsvector
        );
        assert!(!db.backend_capabilities().supports_sqlite_vec);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_schema_is_current_passes_on_a_freshly_migrated_db() {
        // `open_in_memory` runs the migrator as part of `ensure_initialized`,
        // so once it succeeds we should be at HEAD.
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        db.verify_schema_is_current()
            .await
            .expect("verify_schema_is_current should pass on a freshly migrated DB");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_schema_is_current_fails_when_a_migration_is_missing() {
        // Migrate to HEAD, then delete the most recent row to fake a
        // binary that ships one migration the DB has not received yet.
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        sqlx::query(
            "DELETE FROM _sqlx_migrations \
             WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)",
        )
        .execute(db.pool())
        .await
        .expect("drop newest migration row");

        let err = db
            .verify_schema_is_current()
            .await
            .expect_err("verify should refuse when binary-known migrations are missing");
        let message = err.to_string();
        assert!(
            message.contains("missing migrations"),
            "expected error to call out the missing version list, got: {message}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_in_memory_generates_unique_isolation_containers() {
        let a = Database::open_in_memory().unwrap();
        let b = Database::open_in_memory().unwrap();
        let a_init = a
            .test_branch
            .as_ref()
            .expect("open_in_memory sets test_branch");
        let b_init = b
            .test_branch
            .as_ref()
            .expect("open_in_memory sets test_branch");
        assert_ne!(a_init.test_db, b_init.test_db);
        assert!(a_init.test_db.starts_with("djinn_test_"));
        assert_ne!(a.bootstrap_info().target, b.bootstrap_info().target);
    }
}
