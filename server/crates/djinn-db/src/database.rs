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
        let pool = self.pool.clone();
        self.initialized
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(move || migrations::run(&db_path))
                    .await
                    .expect("migration task panicked")
                    .map_err(|e| DbError::InvalidData(e.to_string()))?;

                backfill_missing_content_hashes(&pool).await?;

                Ok::<(), DbError>(())
            })
            .await?;

        Ok(())
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
}
