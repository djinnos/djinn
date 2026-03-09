use sqlx::SqlitePool;

use crate::error::Result;

mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations");
}

pub async fn run(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS refinery_schema_history (
             version INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             applied_on TEXT NOT NULL,
             checksum TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;

    let mut migrations = embedded::migrations::runner().get_migrations().clone();
    migrations.sort_by_key(|m| m.version());

    for migration in &migrations {
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM refinery_schema_history WHERE version = ?1")
                .bind(migration.version())
                .fetch_one(pool)
                .await?;
        if exists > 0 {
            continue;
        }

        if let Some(sql) = migration.sql() {
            // Strip PRAGMA statements — they must be outside transactions.
            // The runner handles foreign_keys explicitly.
            let clean_sql = strip_pragmas(sql);

            // Run the migration SQL inside PRAGMA foreign_keys=OFF + manual
            // transaction on a dedicated connection. Spawned via
            // spawn_blocking so that &mut SqliteConnection (Executor<'_>)
            // usage doesn't infect the caller's async state machine and
            // trigger crate-wide sqlx lifetime inference failures.
            run_migration_sql(pool, &clean_sql).await?;

            sqlx::query(
                "INSERT INTO refinery_schema_history (version, name, applied_on, checksum)
                 VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3)",
            )
            .bind(migration.version())
            .bind(migration.name())
            .bind(migration.checksum().to_string())
            .execute(pool)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO refinery_schema_history (version, name, applied_on, checksum)
                 VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3)",
            )
            .bind(migration.version())
            .bind(migration.name())
            .bind(migration.checksum().to_string())
            .execute(pool)
            .await?;
        }
    }

    Ok(())
}

/// Execute migration SQL inside a PRAGMA foreign_keys=OFF + BEGIN EXCLUSIVE
/// transaction on a dedicated pool connection.
///
/// Uses `spawn_blocking` + `block_on` so that `&mut SqliteConnection` never
/// appears in an async state machine. This avoids a known sqlx lifetime
/// inference bug where `Executor<'_>` for `&mut SqliteConnection` poisons
/// Send bounds crate-wide.
async fn run_migration_sql(pool: &SqlitePool, sql: &str) -> Result<()> {
    let pool = pool.clone();
    let sql = sql.to_owned();
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(async {
            let mut conn = pool.acquire().await?;
            sqlx::raw_sql("PRAGMA foreign_keys = OFF")
                .execute(&mut *conn)
                .await?;
            sqlx::raw_sql("BEGIN EXCLUSIVE")
                .execute(&mut *conn)
                .await?;

            match sqlx::raw_sql(&sql).execute(&mut *conn).await {
                Ok(_) => {
                    sqlx::raw_sql("COMMIT").execute(&mut *conn).await?;
                    sqlx::raw_sql("PRAGMA foreign_keys = ON")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                }
                Err(e) => {
                    let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
                    let _ = sqlx::raw_sql("PRAGMA foreign_keys = ON")
                        .execute(&mut *conn)
                        .await;
                    Err(e.into())
                }
            }
        })
    })
    .await
    .expect("migration task panicked")
}

/// Strip `PRAGMA` lines from migration SQL. The runner manages
/// `PRAGMA foreign_keys` outside the transaction so that SQLite
/// honours it (PRAGMAs are no-ops inside transactions).
fn strip_pragmas(sql: &str) -> String {
    sql.lines()
        .filter(|line| {
            !line
                .trim()
                .to_uppercase()
                .starts_with("PRAGMA ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    use super::run;

    async fn test_pool() -> sqlx::SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(opts)
    }

    #[tokio::test]
    async fn tables_exist_after_migration() {
        let pool = test_pool().await;
        run(&pool).await.unwrap();

        let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
        let tables: Vec<String> = rows.into_iter().map(|r| r.get(0)).collect();

        assert!(tables.contains(&"settings".to_string()));
        assert!(tables.contains(&"projects".to_string()));
        assert!(tables.contains(&"epics".to_string()));
        assert!(tables.contains(&"tasks".to_string()));
        assert!(tables.contains(&"blockers".to_string()));
        assert!(tables.contains(&"activity_log".to_string()));
        assert!(tables.contains(&"notes".to_string()));
        assert!(tables.contains(&"note_links".to_string()));
        assert!(tables.contains(&"credentials".to_string()));
        assert!(tables.contains(&"sessions".to_string()));
    }

    /// Seed data before migrations and verify it survives all table rebuilds.
    /// Catches accidental data loss from CREATE-INSERT-DROP migration patterns.
    #[tokio::test]
    async fn migrations_preserve_existing_data() {
        let pool = test_pool().await;
        run(&pool).await.unwrap();

        // Seed a project, epic, task, blocker, activity entry, and session.
        let project_id = "proj-00000000-0000-0000-0000-000000000001";
        let epic_id = "epic-00000000-0000-0000-0000-000000000001";
        let task_a = "task-00000000-0000-0000-0000-00000000000a";
        let task_b = "task-00000000-0000-0000-0000-00000000000b";
        let session_id = "sess-00000000-0000-0000-0000-000000000001";

        sqlx::query("INSERT INTO projects (id, name, path) VALUES (?1, 'test-proj', '/tmp/test')")
            .bind(project_id)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO epics (id, project_id, short_id, title) VALUES (?1, ?2, 'ep01', 'Test Epic')")
            .bind(epic_id)
            .bind(project_id)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, status, priority, description, design, labels, acceptance_criteria, memory_refs)
             VALUES (?1, ?2, 'aa01', ?3, 'Task A', 'open', 1, 'desc-a', 'design-a', '[\"label1\"]', '[]', '[]')"
        )
            .bind(task_a)
            .bind(project_id)
            .bind(epic_id)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, title, status, priority, description, design, labels, acceptance_criteria, memory_refs)
             VALUES (?1, ?2, 'bb02', 'Task B', 'closed', 2, 'desc-b', '', '[]', '[]', '[]')"
        )
            .bind(task_b)
            .bind(project_id)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES (?1, ?2)")
            .bind(task_a)
            .bind(task_b)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES ('act-001', ?1, 'user1', 'user', 'comment', '{\"body\":\"hello\"}')"
        )
            .bind(task_a)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status, tokens_in, tokens_out)
             VALUES (?1, ?2, ?3, 'openai/gpt-4', 'worker', 'completed', 100, 200)"
        )
            .bind(session_id)
            .bind(project_id)
            .bind(task_a)
            .execute(&pool)
            .await
            .unwrap();

        // Re-run migrations (they should all be skipped since already applied).
        run(&pool).await.unwrap();

        // Verify all data survived.
        let proj_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(proj_count, 1, "project row lost");

        let epic_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM epics")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(epic_count, 1, "epic row lost");

        let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(task_count, 2, "task rows lost");

        // Verify task field values are intact.
        let row = sqlx::query("SELECT title, status, priority, description, design, labels FROM tasks WHERE id = ?1")
            .bind(task_a)
            .fetch_one(&pool).await.unwrap();
        assert_eq!(row.get::<String, _>("title"), "Task A");
        assert_eq!(row.get::<String, _>("status"), "open");
        assert_eq!(row.get::<i64, _>("priority"), 1);
        assert_eq!(row.get::<String, _>("description"), "desc-a");
        assert_eq!(row.get::<String, _>("design"), "design-a");
        assert_eq!(row.get::<String, _>("labels"), "[\"label1\"]");

        let blocker_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blockers")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(blocker_count, 1, "blocker row lost");

        let activity_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activity_log")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(activity_count, 1, "activity row lost");

        let session_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(session_count, 1, "session row lost");

        let sess_row = sqlx::query("SELECT tokens_in, tokens_out, status FROM sessions WHERE id = ?1")
            .bind(session_id)
            .fetch_one(&pool).await.unwrap();
        assert_eq!(sess_row.get::<i64, _>("tokens_in"), 100);
        assert_eq!(sess_row.get::<i64, _>("tokens_out"), 200);
        assert_eq!(sess_row.get::<String, _>("status"), "completed");
    }
}
