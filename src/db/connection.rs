use std::path::Path;

use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::db::migrations;
use crate::error::Result;

/// Wraps a rusqlite `Connection` behind a tokio `Mutex` for async access.
///
/// All database operations are performed via `spawn_blocking` to avoid
/// blocking the Tokio runtime. A single `Database` instance represents
/// one writer per process.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (or create) the database at `path`, auto-creating parent dirs.
    /// Applies all required PRAGMAs after opening.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory database for tests.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        apply_pragmas(&conn)?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Run a closure with exclusive access to the connection, off the async runtime.
    pub async fn call<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let guard = self.conn.lock().await;
        // SAFETY: we hold the Mutex across the spawn_blocking call.
        // The Connection is not Send, so we use an unsafe trick to move
        // a raw pointer into the blocking closure. The Mutex guarantee
        // ensures exclusive access for the lifetime of the closure.
        let ptr = &*guard as *const Connection as usize;
        let result = tokio::task::spawn_blocking(move || {
            // SAFETY: pointer is valid — we hold the lock in the outer scope.
            let conn = unsafe { &*(ptr as *const Connection) };
            f(conn)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pragmas_applied() {
        let db = Database::open_in_memory().unwrap();

        db.call(|conn| {
            let journal: String =
                conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            // In-memory databases use "memory" journal mode, not WAL.
            // WAL is only meaningful for file-backed databases.
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
}
