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

    #[tokio::test]
    async fn tables_exist_after_migration() {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(opts);

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
}
