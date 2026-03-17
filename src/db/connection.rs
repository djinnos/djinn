pub use djinn_db::Database;

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
        let dir = tempfile::tempdir().unwrap();
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
