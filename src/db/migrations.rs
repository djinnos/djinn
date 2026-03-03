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

    for migration in embedded::migrations::runner().get_migrations() {
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM refinery_schema_history WHERE version = ?1")
                .bind(migration.version())
                .fetch_one(pool)
                .await?;
        if exists > 0 {
            continue;
        }

        if let Some(sql) = migration.sql() {
            sqlx::raw_sql(sql).execute(pool).await?;
        }

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

    Ok(())
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
    }
}
