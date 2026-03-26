use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Executor, SqlitePool};
use tokio::sync::OnceCell;

use crate::error::{DbError, DbResult};
use crate::migrations;

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
    db_path: std::path::PathBuf,
    readonly: bool,
    initialized: Arc<OnceCell<()>>,
}

impl Database {
    /// Open (or create) the database at `path`, auto-creating parent dirs.
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DbError::InvalidData(e.to_string()))?;
        }

        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    apply_pragmas(conn).await?;
                    Ok(())
                })
            })
            .connect_lazy_with(opts);

        Ok(Self {
            pool,
            db_path: path.to_path_buf(),
            readonly: false,
            initialized: Arc::new(OnceCell::new()),
        })
    }

    /// Open the database at `path` in read-only mode.
    pub fn open_readonly(path: &Path) -> DbResult<Self> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}?mode=ro", path.display()))?
            .read_only(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    apply_pragmas_readonly(conn).await?;
                    Ok(())
                })
            })
            .connect_lazy_with(opts);

        Ok(Self {
            pool,
            db_path: path.to_path_buf(),
            readonly: true,
            initialized: Arc::new(OnceCell::new()),
        })
    }

    /// Open a temporary database for tests.
    ///
    /// Uses a temp file so that both rusqlite (for refinery migrations) and
    /// sqlx can access the same database.
    pub fn open_in_memory() -> DbResult<Self> {
        let base = workspace_test_tmp_dir()?;
        let tmp = base.join(format!("djinn-test-{}.db", uuid::Uuid::now_v7()));
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", tmp.display()))?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(300))
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    apply_pragmas(conn).await?;
                    Ok(())
                })
            })
            .connect_lazy_with(opts);

        Ok(Self {
            pool,
            db_path: tmp,
            readonly: false,
            initialized: Arc::new(OnceCell::new()),
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn ensure_initialized(&self) -> DbResult<()> {
        if self.readonly {
            return Ok(());
        }

        let db_path = self.db_path.clone();
        self.initialized
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(move || migrations::run(&db_path))
                    .await
                    .expect("migration task panicked")
                    .map_err(|e| DbError::InvalidData(e.to_string()))?;
                Ok::<(), DbError>(())
            })
            .await?;

        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn test_tempdir() -> DbResult<tempfile::TempDir> {
    let base = workspace_test_tmp_dir()?;
    tempfile::Builder::new()
        .prefix("djinn-test-")
        .tempdir_in(base)
        .map_err(|e| DbError::InvalidData(e.to_string()))
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
}
