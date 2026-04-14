use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::ffi::sqlite3_auto_extension;
use serde::{Deserialize, Serialize};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Executor, MySqlPool, SqlitePool};
use tokio::sync::OnceCell;
use tracing::warn;

use crate::error::{DbError, DbResult};
use crate::migrations;

const NOTE_EMBEDDING_DIMENSIONS: usize = 768;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseBackendKind {
    Sqlite,
    Mysql,
}

impl DatabaseBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
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
    Sqlite(SqliteDatabaseConfig),
    Mysql(MysqlDatabaseConfig),
}

impl DatabaseConnectConfig {
    pub fn backend_kind(&self) -> DatabaseBackendKind {
        match self {
            Self::Sqlite(_) => DatabaseBackendKind::Sqlite,
            Self::Mysql(_) => DatabaseBackendKind::Mysql,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteDatabaseConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default = "default_true")]
    pub create_if_missing: bool,
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

#[derive(Clone, Debug)]
pub enum DatabasePool {
    Sqlite(SqlitePool),
    Mysql(MySqlPool),
}

impl DatabasePool {
    pub fn as_sqlite(&self) -> Option<&SqlitePool> {
        match self {
            Self::Sqlite(pool) => Some(pool),
            Self::Mysql(_) => None,
        }
    }

    pub fn as_mysql(&self) -> Option<&MySqlPool> {
        match self {
            Self::Sqlite(_) => None,
            Self::Mysql(pool) => Some(pool),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_mysql_flavor() -> MysqlBackendFlavor {
    MysqlBackendFlavor::Mysql
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqliteVecStatus {
    pub available: bool,
    pub version: Option<String>,
    pub detail: Option<String>,
}

static SQLITE_VEC_REGISTRATION: OnceLock<Result<(), String>> = OnceLock::new();
static SQLITE_VEC_DISABLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
pub struct Database {
    pool: DatabasePool,
    db_path: Option<std::path::PathBuf>,
    readonly: bool,
    bootstrap: DatabaseBootstrapInfo,
    capabilities: DatabaseBackendCapabilities,
    initialized: Arc<OnceCell<()>>,
    sqlite_vec_status: Arc<OnceCell<SqliteVecStatus>>,
}

impl Database {
    /// Open (or create) the database at `path`, auto-creating parent dirs.
    pub fn open(path: &Path) -> DbResult<Self> {
        Self::open_with_config(DatabaseConnectConfig::Sqlite(SqliteDatabaseConfig {
            path: path.to_path_buf(),
            readonly: false,
            create_if_missing: true,
        }))
    }

    /// Open the database at `path` in read-only mode.
    pub fn open_readonly(path: &Path) -> DbResult<Self> {
        Self::open_with_config(DatabaseConnectConfig::Sqlite(SqliteDatabaseConfig {
            path: path.to_path_buf(),
            readonly: true,
            create_if_missing: false,
        }))
    }

    /// Open a temporary database for tests.
    ///
    /// Uses a temp file so that both rusqlite (for refinery migrations) and
    /// sqlx can access the same database.
    pub fn open_in_memory() -> DbResult<Self> {
        let base = workspace_test_tmp_dir()?;
        let tmp = base.join(format!("djinn-test-{}.db", uuid::Uuid::now_v7()));
        Self::open_sqlite(
            &SqliteDatabaseConfig {
                path: tmp,
                readonly: false,
                create_if_missing: true,
            },
            1,
        )
    }

    /// Open a database using an explicit backend selection seam.
    pub fn open_with_config(config: DatabaseConnectConfig) -> DbResult<Self> {
        match config {
            DatabaseConnectConfig::Sqlite(sqlite) => Self::open_sqlite(&sqlite, 8),
            DatabaseConnectConfig::Mysql(mysql) => Self::open_mysql(&mysql, 8),
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        self.pool.as_sqlite().expect(
            "sqlite pool requested for a mysql/dolt runtime; branch on backend_capabilities() before using sqlite-specific repository paths",
        )
    }

    pub fn mysql_pool(&self) -> Option<&MySqlPool> {
        self.pool.as_mysql()
    }

    pub fn pool_kind(&self) -> &DatabasePool {
        &self.pool
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

    fn open_sqlite(config: &SqliteDatabaseConfig, max_connections: u32) -> DbResult<Self> {
        if !config.readonly
            && config.create_if_missing
            && let Some(parent) = config.path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|e| DbError::InvalidData(e.to_string()))?;
        }

        let dsn = if config.readonly {
            format!("sqlite://{}?mode=ro", config.path.display())
        } else {
            format!("sqlite://{}", config.path.display())
        };

        let mut opts = SqliteConnectOptions::from_str(&dsn)?
            .read_only(config.readonly)
            .foreign_keys(true);
        if !config.readonly {
            opts = opts.create_if_missing(config.create_if_missing);
        }

        let mut pool_opts = SqlitePoolOptions::new().max_connections(max_connections);
        if max_connections == 1 {
            pool_opts = pool_opts.acquire_timeout(std::time::Duration::from_secs(300));
        }
        let readonly = config.readonly;
        let pool = pool_opts
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    if readonly {
                        apply_pragmas_readonly(conn).await?;
                    } else {
                        apply_pragmas(conn).await?;
                    }
                    Ok(())
                })
            })
            .connect_lazy_with(opts);

        Ok(Self {
            pool: DatabasePool::Sqlite(pool),
            db_path: Some(config.path.clone()),
            readonly: config.readonly,
            bootstrap: DatabaseBootstrapInfo {
                backend_kind: DatabaseBackendKind::Sqlite,
                backend_label: "sqlite".to_owned(),
                target: config.path.display().to_string(),
                readonly: config.readonly,
            },
            capabilities: DatabaseBackendCapabilities {
                backend_kind: DatabaseBackendKind::Sqlite,
                backend_label: "sqlite".to_owned(),
                lexical_search: NoteSearchBackend::SqliteFts5,
                note_vector_backend: NoteVectorBackend::SqliteVec,
                supports_sqlite_pragmas: true,
                supports_sqlite_vec: true,
                supports_branching_metadata: false,
                supports_readonly_connection_mode: true,
            },
            initialized: Arc::new(OnceCell::new()),
            sqlite_vec_status: Arc::new(OnceCell::new()),
        })
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
            pool: DatabasePool::Mysql(pool),
            db_path: None,
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
            sqlite_vec_status: Arc::new(OnceCell::new()),
        })
    }

    pub async fn ensure_initialized(&self) -> DbResult<()> {
        if self.readonly {
            return Ok(());
        }

        if let Some(pool) = self.mysql_pool().cloned() {
            self.initialized
                .get_or_try_init(|| async move {
                    let mut conn = pool.acquire().await?;
                    sqlx::query("SELECT 1").execute(&mut *conn).await?;
                    Ok::<(), DbError>(())
                })
                .await?;
            return Ok(());
        }

        let db_path = self
            .db_path
            .clone()
            .expect("sqlite databases always retain a filesystem path");
        let pool = self.pool().clone();
        let sqlite_vec_status = self.sqlite_vec_status.clone();
        self.initialized
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(move || migrations::run(&db_path))
                    .await
                    .expect("migration task panicked")
                    .map_err(|e| DbError::InvalidData(e.to_string()))?;

                backfill_missing_content_hashes(&pool).await?;
                let status = initialize_sqlite_vec(&pool).await;
                let _ = sqlite_vec_status.set(status);

                Ok::<(), DbError>(())
            })
            .await?;

        Ok(())
    }

    pub async fn sqlite_vec_status(&self) -> DbResult<SqliteVecStatus> {
        if !self.capabilities.supports_sqlite_vec {
            return Ok(SqliteVecStatus {
                available: false,
                version: None,
                detail: Some(format!(
                    "backend `{}` uses {:?} vectors instead of sqlite-vec",
                    self.bootstrap.backend_label, self.capabilities.note_vector_backend
                )),
            });
        }
        self.ensure_initialized().await?;
        Ok(self
            .sqlite_vec_status
            .get()
            .cloned()
            .unwrap_or_else(|| SqliteVecStatus {
                available: false,
                version: None,
                detail: Some("sqlite-vec initialization was not attempted".to_owned()),
            }))
    }
}

fn register_sqlite_vec_auto_extension() -> Result<(), String> {
    if SQLITE_VEC_DISABLED.load(Ordering::SeqCst) {
        return Err("sqlite-vec explicitly disabled for this process".to_owned());
    }

    SQLITE_VEC_REGISTRATION
        .get_or_init(|| unsafe {
            #[allow(clippy::missing_transmute_annotations)]
            let init_fn = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            sqlite3_auto_extension(Some(init_fn));
            Ok(())
        })
        .clone()
}

async fn initialize_sqlite_vec(pool: &SqlitePool) -> SqliteVecStatus {
    if SQLITE_VEC_DISABLED.load(Ordering::SeqCst) {
        let detail = "sqlite-vec explicitly disabled for this process".to_owned();
        warn!(error = %detail, "sqlite-vec registration unavailable; semantic vector queries disabled");
        return SqliteVecStatus {
            available: false,
            version: None,
            detail: Some(detail),
        };
    }

    if let Err(error) = register_sqlite_vec_auto_extension() {
        warn!(error = %error, "sqlite-vec registration unavailable; semantic vector queries disabled");
        return SqliteVecStatus {
            available: false,
            version: None,
            detail: Some(error),
        };
    }

    let version = match sqlx::query_scalar::<_, String>("SELECT vec_version()")
        .fetch_one(pool)
        .await
    {
        Ok(version) => version,
        Err(error) => {
            let detail = error.to_string();
            warn!(error = %detail, "sqlite-vec could not be activated on this database connection");
            return SqliteVecStatus {
                available: false,
                version: None,
                detail: Some(detail),
            };
        }
    };

    let create_sql = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings_vec USING vec0(\
            note_id TEXT PRIMARY KEY, \
            embedding float[{NOTE_EMBEDDING_DIMENSIONS}] distance_metric=cosine\
        )"
    );

    if let Err(error) = sqlx::query(&create_sql).execute(pool).await {
        let detail = error.to_string();
        warn!(error = %detail, "sqlite-vec loaded but vec0 storage could not be initialized");
        return SqliteVecStatus {
            available: false,
            version: Some(version),
            detail: Some(detail),
        };
    }

    SqliteVecStatus {
        available: true,
        version: Some(version),
        detail: None,
    }
}

/// Backfill NULL `content_hash` values for any notes that lack them.
///
/// Runs during initialization so legacy notes created before the
/// `content_hash` column was populated get a deterministic hash.
async fn backfill_missing_content_hashes(pool: &SqlitePool) -> DbResult<()> {
    use crate::note_hash::note_content_hash;

    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, content FROM notes WHERE content_hash IS NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::InvalidData(e.to_string()))?;

    if rows.is_empty() {
        return Ok(());
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::InvalidData(e.to_string()))?;
    for (id, content) in &rows {
        let hash = note_content_hash(content);
        sqlx::query("UPDATE notes SET content_hash = ?2 WHERE id = ?1")
            .bind(id)
            .bind(hash)
            .execute(&mut *tx)
            .await
            .map_err(|e| DbError::InvalidData(e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(|e| DbError::InvalidData(e.to_string()))?;

    Ok(())
}

pub(crate) fn test_tempdir() -> DbResult<tempfile::TempDir> {
    let base = workspace_test_tmp_dir()?;
    tempfile::Builder::new()
        .prefix("djinn-test-")
        .tempdir_in(base)
        .map_err(|e| DbError::InvalidData(e.to_string()))
}

#[cfg(test)]
pub(crate) fn create_legacy_note_fixture_db(path: &Path) -> DbResult<LegacyNoteFixture> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DbError::InvalidData(e.to_string()))?;
    }

    if path.exists() {
        std::fs::remove_file(path).map_err(|e| DbError::InvalidData(e.to_string()))?;
    }

    migrations::run_until(path, "add_note_content_hash")
        .map_err(|e| DbError::InvalidData(e.to_string()))?;

    let conn = rusqlite::Connection::open(path).map_err(|e| DbError::InvalidData(e.to_string()))?;

    let project_id = uuid::Uuid::nil().to_string();
    let note_id = uuid::Uuid::from_u128(1).to_string();
    let project_path = path.with_extension("project");
    if project_path.exists() {
        std::fs::remove_dir_all(&project_path).map_err(|e| DbError::InvalidData(e.to_string()))?;
    }
    std::fs::create_dir_all(&project_path).map_err(|e| DbError::InvalidData(e.to_string()))?;
    let note_file = project_path.join("legacy-note.md");
    let note_content = "Legacy fixture body\n";
    std::fs::write(&note_file, note_content).map_err(|e| DbError::InvalidData(e.to_string()))?;

    conn.execute(
        "INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            project_id,
            "legacy-project",
            project_path.display().to_string()
        ],
    )
    .map_err(|e| DbError::InvalidData(e.to_string()))?;

    conn.execute(
        "INSERT INTO notes (
            id, project_id, permalink, title, file_path, storage, note_type, folder, tags, content
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            note_id,
            project_id,
            "reference/legacy-note",
            "Legacy Note",
            note_file.display().to_string(),
            "file",
            "reference",
            "reference",
            "[]",
            note_content,
        ],
    )
    .map_err(|e| DbError::InvalidData(e.to_string()))?;

    Ok(LegacyNoteFixture { note_id })
}

#[cfg(test)]
pub(crate) struct LegacyNoteFixture {
    pub note_id: String,
}

fn workspace_test_tmp_dir() -> DbResult<PathBuf> {
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

fn workspace_root_from_current_dir() -> Option<PathBuf> {
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

async fn apply_pragmas(conn: &mut sqlx::sqlite::SqliteConnection) -> sqlx::Result<()> {
    conn.execute("PRAGMA journal_mode = WAL;").await?;
    conn.execute("PRAGMA busy_timeout = 30000;").await?;
    conn.execute("PRAGMA synchronous = NORMAL;").await?;
    conn.execute("PRAGMA foreign_keys = ON;").await?;
    conn.execute("PRAGMA cache_size = -64000;").await?;
    Ok(())
}

/// Read-only connections skip journal_mode and synchronous.
async fn apply_pragmas_readonly(conn: &mut sqlx::sqlite::SqliteConnection) -> sqlx::Result<()> {
    conn.execute("PRAGMA busy_timeout = 30000;").await?;
    conn.execute("PRAGMA foreign_keys = ON;").await?;
    conn.execute("PRAGMA cache_size = -64000;").await?;
    Ok(())
}

/// Default database path: `~/.djinn/djinn.db`.
pub fn default_db_path() -> std::path::PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".djinn")
        .join("djinn.db")
}

#[cfg(test)]
pub(crate) fn set_sqlite_vec_disabled_for_tests(disabled: bool) {
    SQLITE_VEC_DISABLED.store(disabled, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pragmas_applied() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        let row = sqlx::query("PRAGMA journal_mode")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let journal: String = row.get(0);
        assert!(
            journal == "wal" || journal == "memory",
            "unexpected journal_mode: {journal}"
        );

        let timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(timeout, 30000);

        let sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(sync, 1);

        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_file_db_and_readonly_reader() {
        let dir = crate::database::test_tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let writer = Database::open(&db_path).unwrap();
        writer.ensure_initialized().await.unwrap();
        sqlx::query("CREATE TABLE rw_test (id TEXT PRIMARY KEY, val TEXT)")
            .execute(writer.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO rw_test VALUES ('k1', 'hello')")
            .execute(writer.pool())
            .await
            .unwrap();

        let reader = Database::open_readonly(&db_path).unwrap();
        let val: String = sqlx::query_scalar("SELECT val FROM rw_test WHERE id = 'k1'")
            .fetch_one(reader.pool())
            .await
            .unwrap();
        assert_eq!(val, "hello");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_note_fixture_is_accepted_by_current_initialization() {
        let dir = crate::database::test_tempdir().unwrap();
        let db_path = dir.path().join("legacy.db");
        let fixture = create_legacy_note_fixture_db(&db_path).unwrap();

        let pre_migration_columns: Vec<String> = {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let mut query = conn.prepare("PRAGMA table_info(notes)").unwrap();
            query
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(
            !pre_migration_columns
                .iter()
                .any(|column| column == "content_hash")
        );

        let db = Database::open(&db_path).unwrap();
        db.ensure_initialized().await.unwrap();

        let content_hash: Option<String> =
            sqlx::query_scalar("SELECT content_hash FROM notes WHERE id = ?1")
                .bind(&fixture.note_id)
                .fetch_one(db.pool())
                .await
                .unwrap();

        let normalized_fixture_hash = crate::note_hash::note_content_hash("Legacy fixture body\n");
        assert_eq!(
            content_hash.as_deref(),
            Some(normalized_fixture_hash.as_str()),
            "ensure_initialized should backfill content_hash for legacy notes"
        );

        let migration_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM refinery_schema_history WHERE name = 'add_note_content_hash'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(migration_count, 1);

        let index_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'notes_project_content_hash_idx'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(index_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_vec_status_reports_available_or_graceful_fallback() {
        set_sqlite_vec_disabled_for_tests(false);
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        let status = db.sqlite_vec_status().await.unwrap();
        if status.available {
            let table_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'note_embeddings_vec'",
            )
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(table_count, 1);
            assert!(status.version.is_some());
        } else {
            assert!(status.detail.is_some());
        }
    }

    /// Guard against modifying already-applied migrations.
    ///
    /// Refinery stores a checksum of each migration when it is first applied.
    /// If someone later edits an already-applied .sql file, the embedded
    /// checksum diverges from the DB record and **every** database operation
    /// fails at runtime (see: V20260409000001 incident).
    ///
    /// This test applies all migrations to a fresh DB, then compares the
    /// checksums refinery recorded with the checksums of the embedded files.
    /// A mismatch means a migration was edited after being committed —
    /// split the change into a new migration instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn migration_checksums_are_stable_after_apply() {
        use crate::migrations::embedded_checksums;

        // Apply all migrations to a fresh in-memory database.
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        // Read what refinery recorded in the schema history table.
        // Refinery stores checksum as TEXT in SQLite, so we parse it.
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT version, checksum FROM refinery_schema_history ORDER BY version",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        let applied: Vec<(i64, u64)> = rows
            .into_iter()
            .map(|(v, c)| (v, c.parse::<u64>().expect("checksum should be a u64")))
            .collect();

        // Compare against the embedded (compile-time) checksums.
        let embedded = embedded_checksums();
        for (version, recorded_checksum) in &applied {
            let entry = embedded
                .iter()
                .find(|(v, _, _)| *v == *version)
                .unwrap_or_else(|| {
                    panic!("applied migration V{version} not found in embedded migrations")
                });
            assert_eq!(
                *recorded_checksum, entry.2,
                "Migration V{}_{} has been modified after it was applied! \
                 Do NOT edit existing migrations — create a new one instead. \
                 (recorded checksum {recorded_checksum} != embedded checksum {})",
                version, entry.1, entry.2
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_vec_can_be_policy_disabled_without_breaking_db_use() {
        set_sqlite_vec_disabled_for_tests(true);

        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        let _status = db.sqlite_vec_status().await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM note_embeddings")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);

        set_sqlite_vec_disabled_for_tests(false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mysql_backend_selection_returns_explicit_staging_error() {
        let db = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3306/djinn".to_owned(),
            flavor: MysqlBackendFlavor::Dolt,
        }))
        .expect("mysql/dolt backend should construct a concrete runtime path");

        assert_eq!(db.backend_kind(), DatabaseBackendKind::Mysql);
        assert_eq!(db.bootstrap_info().backend_label, "dolt");
        assert!(db.mysql_pool().is_some());
        assert!(db.pool_kind().as_sqlite().is_none());
        assert_eq!(
            db.backend_capabilities().lexical_search,
            NoteSearchBackend::MysqlFulltext
        );
        assert!(db.backend_capabilities().supports_branching_metadata);
        assert!(!db.backend_capabilities().supports_sqlite_vec);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_backend_selection_preserves_bootstrap_metadata() {
        let dir = crate::database::test_tempdir().unwrap();
        let db_path = dir.path().join("selected.db");

        let db = Database::open_with_config(DatabaseConnectConfig::Sqlite(SqliteDatabaseConfig {
            path: db_path.clone(),
            readonly: false,
            create_if_missing: true,
        }))
        .unwrap();

        assert_eq!(db.backend_kind(), DatabaseBackendKind::Sqlite);
        assert_eq!(db.bootstrap_info().backend_label, "sqlite");
        assert_eq!(db.bootstrap_info().target, db_path.display().to_string());
        assert!(!db.bootstrap_info().readonly);
        assert_eq!(
            db.backend_capabilities().lexical_search,
            NoteSearchBackend::SqliteFts5
        );
        assert!(db.backend_capabilities().supports_sqlite_pragmas);
        assert!(db.backend_capabilities().supports_sqlite_vec);
        assert!(!db.backend_capabilities().supports_branching_metadata);
    }

    #[tokio::test(flavor = "current_thread")]
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
