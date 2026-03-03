use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use tokio::sync::Mutex;

use crate::db::migrations;
use crate::error::Result;

/// Default database path: `~/.djinn/djinn.db`.
pub fn default_db_path() -> std::path::PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".djinn")
        .join("djinn.db")
}

/// Wraps a rusqlite `Connection` behind an `Arc<Mutex>` for async access.
///
/// Cheaply cloneable — clones share the same underlying connection.
/// All database operations are performed via `spawn_blocking` to avoid
/// blocking the Tokio runtime. A single writer connection per process.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open (or create) the database at `path`, auto-creating parent dirs.
    /// Applies PRAGMAs then runs all pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open the database at `path` in read-only mode.
    ///
    /// Used by the desktop process to read while the server holds the writer.
    /// WAL mode allows concurrent readers without blocking the writer.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        apply_pragmas_readonly(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database for tests.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        apply_pragmas(&conn)?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a read-only closure with access to the connection.
    pub async fn call<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let guard = self.conn.lock().await;
        // SAFETY: we hold the Mutex across the spawn_blocking call.
        // Connection is not Send, so we transmit a raw pointer. The Mutex
        // guarantee ensures exclusive access for the lifetime of the closure.
        let ptr = &*guard as *const Connection as usize;
        let result = tokio::task::spawn_blocking(move || {
            let conn = unsafe { &*(ptr as *const Connection) };
            f(conn)
        })
        .await
        .expect("spawn_blocking panicked");
        drop(guard);
        result
    }

    /// Run a write closure inside a `BEGIN IMMEDIATE` transaction.
    ///
    /// `BEGIN IMMEDIATE` acquires the write lock immediately rather than
    /// deferring to first write statement, preventing SQLITE_BUSY from
    /// mid-transaction lock promotion. Auto-commits on `Ok`, rolls back
    /// on `Err`.
    pub async fn write<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let guard = self.conn.lock().await;
        let ptr = &*guard as *const Connection as usize;
        let result = tokio::task::spawn_blocking(move || {
            let conn = unsafe { &*(ptr as *const Connection) };
            conn.execute_batch("BEGIN IMMEDIATE")?;
            match f(conn) {
                Ok(val) => {
                    conn.execute_batch("COMMIT")?;
                    Ok(val)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
        .await
        .expect("spawn_blocking panicked");
        drop(guard);
        result
    }
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -64000;",
    )?;
    Ok(())
}

/// Read-only connections skip journal_mode (cannot change it) and synchronous.
fn apply_pragmas_readonly(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -64000;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pragmas_applied() {
        let db = Database::open_in_memory().unwrap();

        db.call(|conn| {
            let journal: String =
                conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            assert!(
                journal == "wal" || journal == "memory",
                "unexpected journal_mode: {journal}"
            );

            let timeout: i64 =
                conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
            assert_eq!(timeout, 5000);

            let sync: i64 =
                conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            assert_eq!(sync, 1); // NORMAL = 1

            let fk: i64 =
                conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            assert_eq!(fk, 1);

            let cache: i64 =
                conn.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
            assert_eq!(cache, -64000);

            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_commits_on_success() {
        let db = Database::open_in_memory().unwrap();

        db.write(|conn| {
            conn.execute(
                "CREATE TABLE test_write (id TEXT PRIMARY KEY)",
                [],
            )?;
            conn.execute("INSERT INTO test_write VALUES ('a')", [])?;
            Ok(())
        })
        .await
        .unwrap();

        // Verify the data persisted (committed).
        db.call(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM test_write", [], |r| r.get(0))?;
            assert_eq!(count, 1);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_rolls_back_on_error() {
        let db = Database::open_in_memory().unwrap();

        db.write(|conn| {
            conn.execute(
                "CREATE TABLE test_rollback (id TEXT PRIMARY KEY)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // This write should roll back.
        let result: Result<()> = db
            .write(|conn| {
                conn.execute("INSERT INTO test_rollback VALUES ('a')", [])?;
                Err(crate::error::Error::Internal("forced error".into()))
            })
            .await;
        assert!(result.is_err());

        // Table exists but row should not (rolled back).
        db.call(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM test_rollback",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 0, "row should have been rolled back");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn uuidv7_is_sortable() {
        let id1 = uuid::Uuid::now_v7();
        let id2 = uuid::Uuid::now_v7();
        assert!(id2 >= id1, "UUIDv7 should be monotonically sortable");
    }

    #[tokio::test]
    async fn open_file_db_and_readonly_reader() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Writer opens and creates the DB.
        let writer = Database::open(&db_path).unwrap();
        writer
            .write(|conn| {
                conn.execute(
                    "CREATE TABLE rw_test (id TEXT PRIMARY KEY, val TEXT)",
                    [],
                )?;
                conn.execute("INSERT INTO rw_test VALUES ('k1', 'hello')", [])?;
                Ok(())
            })
            .await
            .unwrap();

        // Reader opens the same file read-only.
        let reader = Database::open_readonly(&db_path).unwrap();
        reader
            .call(|conn| {
                let val: String = conn.query_row(
                    "SELECT val FROM rw_test WHERE id = 'k1'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(val, "hello");
                Ok(())
            })
            .await
            .unwrap();
    }
}
