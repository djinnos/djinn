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
}

impl Database {
    /// Open a database using an explicit backend selection seam.
    pub fn open_with_config(config: DatabaseConnectConfig) -> DbResult<Self> {
        match config {
            DatabaseConnectConfig::Mysql(mysql) => Self::open_mysql(&mysql, 8),
        }
    }

    /// Open a fresh MySQL/Dolt database for tests.
    ///
    /// Generates a unique database name (`djinn_test_{uuid_simple}`) under the
    /// MySQL server pointed to by `DJINN_TEST_MYSQL_URL` (defaults to
    /// `mysql://root@127.0.0.1:3307` — the isolated test Dolt from
    /// `docker compose --profile tests up dolt-test`). The default is
    /// deliberately NOT the dev Dolt on :3306 — tests pollute the server
    /// with thousands of `djinn_test_*` databases that balloon its RAM
    /// footprint. Overriding to :3306 requires an explicit env var.
    /// Returns a `Database` configured against that fresh DB;
    /// `ensure_initialized()` will `CREATE DATABASE` and run the full
    /// MySQL migration chain on first use.
    ///
    /// ## Cleanup
    ///
    /// Test databases are **leaked** — they persist on the Dolt server after
    /// the test process exits. Run `make test-db-clean` (server/Makefile) to
    /// drop all `djinn_test_*` databases between runs.
    ///
    /// Rationale: `Database` is `Clone`, so a `Drop` impl would fire on every
    /// clone going out of scope — either prematurely dropping the live DB or
    /// requiring refcounted guard wrapping that every one of ~230 call sites
    /// would have to opt into. Leak-plus-cleanup is simpler and matches how
    /// most MySQL integration suites handle ephemeral DBs.
    pub fn open_in_memory() -> DbResult<Self> {
        let base_url = std::env::var("DJINN_TEST_MYSQL_URL")
            .unwrap_or_else(|_| "mysql://root@127.0.0.1:3307".to_owned());
        let db_name = format!(
            "djinn_test_{}",
            uuid::Uuid::now_v7().simple()
        );
        // Strip any trailing database path from the base URL before appending
        // the fresh test DB name.
        let trimmed = base_url.trim_end_matches('/');
        let (server_prefix, _existing_db) = match trimmed.strip_prefix("mysql://") {
            Some(after_scheme) => {
                if let Some(slash) = after_scheme.find('/') {
                    (
                        format!("mysql://{}", &after_scheme[..slash]),
                        Some(&after_scheme[slash + 1..]),
                    )
                } else {
                    (format!("mysql://{after_scheme}"), None)
                }
            }
            None => (trimmed.to_owned(), None),
        };
        let url = format!("{server_prefix}/{db_name}");

        Self::open_mysql(
            &MysqlDatabaseConfig {
                url,
                flavor: MysqlBackendFlavor::Dolt,
            },
            4,
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
        let opts = MySqlConnectOptions::from_str(&config.url)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(max_connections)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query("SET SESSION sql_mode = CONCAT(@@sql_mode, ',STRICT_ALL_TABLES')")
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
        })
    }

    /// Legacy stub retained so sqlite-vec-era embedding tests still compile.
    ///
    /// MySQL/Dolt has no sqlite-vec equivalent — vector search is handled by
    /// Qdrant. This always returns "unavailable" so tests that gate on the
    /// extension status take their fallback path.
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
        self.initialized
            .get_or_try_init(|| async move {
                migrations::ensure_mysql_database_exists(&url).await?;
                sqlx::migrate!("./migrations_mysql")
                    .run(&pool)
                    .await
                    .map_err(|e: sqlx::migrate::MigrateError| {
                        DbError::InvalidData(e.to_string())
                    })?;
                Ok::<(), DbError>(())
            })
            .await?;
        Ok(())
    }
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
    async fn open_in_memory_generates_unique_db_names() {
        let a = Database::open_in_memory().unwrap();
        let b = Database::open_in_memory().unwrap();
        assert_ne!(a.bootstrap_info().target, b.bootstrap_info().target);
        assert!(a.bootstrap_info().target.contains("djinn_test_"));
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
