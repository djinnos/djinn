use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use tokio::sync::OnceCell;

use crate::error::{DbError, DbResult};
use crate::migrations;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseBackendKind {
    Mysql,
}

impl DatabaseBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mysql => "mysql",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MysqlBackendFlavor {
    Mysql,
    Dolt,
}

impl MysqlBackendFlavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mysql => "mysql",
            Self::Dolt => "dolt",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum DatabaseConnectConfig {
    Mysql(MysqlDatabaseConfig),
}

impl DatabaseConnectConfig {
    pub fn backend_kind(&self) -> DatabaseBackendKind {
        match self {
            Self::Mysql(_) => DatabaseBackendKind::Mysql,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MysqlDatabaseConfig {
    pub url: String,
    #[serde(default = "default_mysql_flavor")]
    pub flavor: MysqlBackendFlavor,
}

impl MysqlDatabaseConfig {
    pub fn display_backend(&self) -> &'static str {
        self.flavor.as_str()
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
    MysqlFulltext,
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
    pub supports_branching_metadata: bool,
    pub supports_readonly_connection_mode: bool,
}

fn default_mysql_flavor() -> MysqlBackendFlavor {
    MysqlBackendFlavor::Mysql
}

#[derive(Clone, Debug)]
pub struct Database {
    pool: MySqlPool,
    readonly: bool,
    bootstrap: DatabaseBootstrapInfo,
    capabilities: DatabaseBackendCapabilities,
    initialized: Arc<OnceCell<()>>,
    test_branch: Option<TestBranchInit>,
}

#[derive(Clone, Debug)]
struct TestBranchInit {
    server_prefix: String,
    shared_db: String,
    branch: String,
}

/// Process-wide guard: the shared `djinn_test` database is created and
/// migrated exactly once per test process, regardless of how many
/// `open_in_memory()` callers fan out. Branch creation is per-instance.
static SHARED_TEST_DB_INIT: OnceCell<()> = OnceCell::const_new();

impl Database {
    /// Open a database using an explicit backend selection seam.
    pub fn open_with_config(config: DatabaseConnectConfig) -> DbResult<Self> {
        match config {
            // Dolt COMMITs can take 30–90s under write contention (merge-based
            // commit path is O(working-set)), so a connection is held ~10×
            // longer per write than a plain MySQL one. A pool of 8 starves
            // almost immediately when mirror-fetcher + KB watcher + MCP tools
            // all have a write in flight. 32 gives reads + concurrent writers
            // headroom without blowing past Dolt's own connection cap.
            DatabaseConnectConfig::Mysql(mysql) => Self::open_mysql(&mysql, 32),
        }
    }

    /// Open an isolated Dolt branch for a single test.
    ///
    /// Uses one shared `djinn_test` database and creates a fresh branch per
    /// caller (`t_{uuid_simple}`). A Dolt branch is an O(1) ref pointer, so
    /// 500 concurrent tests cost ~1x the per-database RAM overhead (commit
    /// graph + table file index) instead of 500x. The prior design created
    /// a full DB per test and pushed the test-Dolt container past its 8 GiB
    /// cap once branches accumulated.
    ///
    /// Connects against `DJINN_TEST_MYSQL_URL` (default
    /// `mysql://root@127.0.0.1:3307`, the isolated test Dolt on port 3307
    /// from `docker compose`). The returned `Database` has a lazy pool: the
    /// branch is not created until the first `ensure_initialized()` call.
    ///
    /// ## Isolation
    ///
    /// Each new connection from the pool runs `USE djinn_test/<branch>` in
    /// `after_connect`, so every query is pinned to this test's branch.
    /// Writes on one branch are invisible to other branches.
    ///
    /// ## Cleanup
    ///
    /// Branches are leaked — they persist until `make test-db-clean` drops
    /// all `t_*` branches, or `make test-db-reset` wipes the container. See
    /// the note on `open_mysql` regarding why a `Drop` impl isn't viable
    /// given `Database: Clone`.
    pub fn open_in_memory() -> DbResult<Self> {
        let base_url = std::env::var("DJINN_TEST_MYSQL_URL")
            .unwrap_or_else(|_| "mysql://root@127.0.0.1:3307".to_owned());
        let server_prefix = strip_server_prefix(&base_url);
        let shared_db = "djinn_test".to_owned();
        let branch = format!("t_{}", uuid::Uuid::now_v7().simple());
        // Pool connects to the shared DB; after_connect pins each conn to
        // the branch via `USE djinn_test/<branch>`.
        let url = format!("{server_prefix}/{shared_db}");

        Self::open_mysql_inner(
            &MysqlDatabaseConfig {
                url,
                flavor: MysqlBackendFlavor::Dolt,
            },
            4,
            Some(TestBranchInit {
                server_prefix,
                shared_db,
                branch,
            }),
        )
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    pub fn mysql_pool(&self) -> Option<&MySqlPool> {
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

    fn open_mysql(config: &MysqlDatabaseConfig, max_connections: u32) -> DbResult<Self> {
        Self::open_mysql_inner(config, max_connections, None)
    }

    fn open_mysql_inner(
        config: &MysqlDatabaseConfig,
        max_connections: u32,
        test_branch: Option<TestBranchInit>,
    ) -> DbResult<Self> {
        let opts = MySqlConnectOptions::from_str(&config.url)?;
        // Precompute the `USE db/branch` statement so the after_connect
        // closure can be shared (no per-connection allocation).
        let use_branch_stmt = test_branch.as_ref().map(|init| {
            // Branch names are UUIDv7 hex (ASCII alphanumerics + underscore),
            // shared_db is a const — safe to interpolate without quoting.
            format!("USE {}/{}", init.shared_db, init.branch)
        });
        let pool = MySqlPoolOptions::new()
            .max_connections(max_connections)
            // Dolt COMMITs can run 30–90s under write contention; the sqlx
            // default acquire_timeout of 30s causes every UI/MCP request
            // queued behind a slow write to hard-fail with "pool timed out"
            // even though the DB is healthy. Raise the ceiling so queued
            // requests wait it out instead of cascading into retry storms.
            .acquire_timeout(std::time::Duration::from_secs(120))
            .after_connect(move |conn, _meta| {
                let use_branch_stmt = use_branch_stmt.clone();
                Box::pin(async move {
                    sqlx::query("SET SESSION sql_mode = CONCAT(@@sql_mode, ',STRICT_ALL_TABLES')")
                        .execute(&mut *conn)
                        .await?;
                    if let Some(stmt) = use_branch_stmt {
                        sqlx::query(&stmt).execute(&mut *conn).await?;
                    }
                    Ok(())
                })
            })
            .connect_lazy_with(opts);

        let backend_label = config.display_backend().to_owned();
        Ok(Self {
            pool,
            readonly: false,
            bootstrap: DatabaseBootstrapInfo {
                backend_kind: DatabaseBackendKind::Mysql,
                backend_label: backend_label.clone(),
                target: config.url.clone(),
                readonly: false,
            },
            capabilities: DatabaseBackendCapabilities {
                backend_kind: DatabaseBackendKind::Mysql,
                backend_label,
                lexical_search: NoteSearchBackend::MysqlFulltext,
                note_vector_backend: NoteVectorBackend::External,
                supports_sqlite_pragmas: false,
                supports_sqlite_vec: false,
                supports_branching_metadata: matches!(config.flavor, MysqlBackendFlavor::Dolt),
                supports_readonly_connection_mode: false,
            },
            initialized: Arc::new(OnceCell::new()),
            test_branch,
        })
    }

    /// Legacy stub retained so sqlite-vec-era embedding tests still compile.
    ///
    /// MySQL/Dolt has no sqlite-vec equivalent — vector search is handled by
    /// Qdrant. This always returns "unavailable" so tests that gate on the
    /// extension status take their fallback path.
    /// Return `true` if a table with the given name exists in the current
    /// database schema.
    ///
    /// Exists as a deliberate test fixture: contract tests verify that the
    /// in-memory test database has its migrations applied. Production code
    /// should not need to probe the schema at runtime.
    pub async fn table_exists(&self, table_name: &str) -> DbResult<bool> {
        self.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = ?",
        )
        .bind(table_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn sqlite_vec_status(&self) -> DbResult<SqliteVecStatus> {
        Ok(SqliteVecStatus {
            available: false,
            version: None,
            detail: Some("sqlite-vec retired with the MySQL/Dolt migration".to_owned()),
        })
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
                    Some(init) => {
                        // Shared djinn_test DB + migrations on `main` —
                        // once per process, regardless of test fan-out.
                        SHARED_TEST_DB_INIT
                            .get_or_try_init(|| {
                                init_shared_test_db(
                                    init.server_prefix.clone(),
                                    init.shared_db.clone(),
                                )
                            })
                            .await?;
                        // Per-instance: create the branch this test will
                        // write against. UUIDv7 names don't collide.
                        create_test_branch(
                            &init.server_prefix,
                            &init.shared_db,
                            &init.branch,
                        )
                        .await?;
                    }
                    None => {
                        migrations::ensure_mysql_database_exists(&url).await?;
                        sqlx::migrate!("./migrations_mysql")
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
    match trimmed.strip_prefix("mysql://") {
        Some(after_scheme) => match after_scheme.find('/') {
            Some(slash) => format!("mysql://{}", &after_scheme[..slash]),
            None => format!("mysql://{after_scheme}"),
        },
        None => trimmed.to_owned(),
    }
}

async fn init_shared_test_db(server_prefix: String, shared_db: String) -> DbResult<()> {
    let url = format!("{server_prefix}/{shared_db}");
    migrations::ensure_mysql_database_exists(&url).await?;
    let pool = MySqlPool::connect(&url)
        .await
        .map_err(DbError::from)?;
    let result = async {
        sqlx::migrate!("./migrations_mysql")
            .run(&pool)
            .await
            .map_err(|e: sqlx::migrate::MigrateError| DbError::InvalidData(e.to_string()))?;
        // Dolt branches fork from the latest *commit*, not the working
        // root. Without this commit, `CALL DOLT_BRANCH('t_x', 'main')`
        // would produce a branch with an empty schema because the
        // migrations live in main's unstaged working root.
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_status")
            .fetch_one(&pool)
            .await
            .map_err(DbError::from)?;
        if pending > 0 {
            sqlx::query("CALL DOLT_ADD('-A')")
                .execute(&pool)
                .await
                .map_err(DbError::from)?;
            sqlx::query("CALL DOLT_COMMIT('-m', 'apply test migrations on main')")
                .execute(&pool)
                .await
                .map_err(DbError::from)?;
        }
        Ok::<(), DbError>(())
    }
    .await;
    pool.close().await;
    result
}

async fn create_test_branch(
    server_prefix: &str,
    shared_db: &str,
    branch: &str,
) -> DbResult<()> {
    use sqlx::{ConnectOptions, Connection, Executor};
    let url = format!("{server_prefix}/{shared_db}");
    let opts = MySqlConnectOptions::from_str(&url)
        .map_err(|e| DbError::InvalidData(format!("invalid mysql url: {e}")))?;
    let mut conn = opts.connect().await.map_err(DbError::from)?;
    let stmt = format!("CALL DOLT_BRANCH('{branch}', 'main')");
    conn.execute(stmt.as_str()).await.map_err(DbError::from)?;
    conn.close().await.map_err(DbError::from)?;
    Ok(())
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
    async fn mysql_backend_selection_metadata_is_dolt_shaped() {
        let db = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3306/djinn".to_owned(),
            flavor: MysqlBackendFlavor::Dolt,
        }))
        .expect("mysql/dolt backend should construct a concrete runtime path");

        assert_eq!(db.backend_kind(), DatabaseBackendKind::Mysql);
        assert_eq!(db.bootstrap_info().backend_label, "dolt");
        assert_eq!(
            db.backend_capabilities().lexical_search,
            NoteSearchBackend::MysqlFulltext
        );
        assert!(db.backend_capabilities().supports_branching_metadata);
        assert!(!db.backend_capabilities().supports_sqlite_vec);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_in_memory_generates_unique_branches() {
        let a = Database::open_in_memory().unwrap();
        let b = Database::open_in_memory().unwrap();
        // Every test shares the same underlying database but gets its own
        // branch. Distinctness is at the branch level, not the URL.
        let a_branch = a
            .test_branch
            .as_ref()
            .expect("open_in_memory sets test_branch");
        let b_branch = b
            .test_branch
            .as_ref()
            .expect("open_in_memory sets test_branch");
        assert_ne!(a_branch.branch, b_branch.branch);
        assert!(a_branch.branch.starts_with("t_"));
        assert_eq!(a_branch.shared_db, "djinn_test");
        assert_eq!(a.bootstrap_info().target, b.bootstrap_info().target);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mysql_and_dolt_capabilities_differ_only_by_branch_metadata() {
        let mysql = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3306/djinn".to_owned(),
            flavor: MysqlBackendFlavor::Mysql,
        }))
        .unwrap();
        let dolt = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3307/djinn".to_owned(),
            flavor: MysqlBackendFlavor::Dolt,
        }))
        .unwrap();

        assert_eq!(
            mysql.backend_capabilities().lexical_search,
            dolt.backend_capabilities().lexical_search
        );
        assert_eq!(
            mysql.backend_capabilities().note_vector_backend,
            dolt.backend_capabilities().note_vector_backend
        );
        assert!(!mysql.backend_capabilities().supports_branching_metadata);
        assert!(dolt.backend_capabilities().supports_branching_metadata);
    }
}
