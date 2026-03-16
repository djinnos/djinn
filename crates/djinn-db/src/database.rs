use std::path::Path;
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
            readonly: true,
            initialized: Arc::new(OnceCell::new()),
        })
    }

    /// Open an in-memory database for tests.
    pub fn open_in_memory() -> DbResult<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
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

        self.initialized
            .get_or_try_init(|| async {
                migrations::run(&self.pool).await?;
                Ok::<(), DbError>(())
            })
            .await?;

        Ok(())
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
