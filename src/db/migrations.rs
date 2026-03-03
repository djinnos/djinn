use sqlx::{Executor, SqlitePool};

use crate::error::Result;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 20260302000001,
        name: "initial_schema",
        sql: include_str!("../../migrations/V20260302000001__initial_schema.sql"),
    },
    Migration {
        version: 20260303000001,
        name: "task_board",
        sql: include_str!("../../migrations/V20260303000001__task_board.sql"),
    },
    Migration {
        version: 20260303000002,
        name: "notes",
        sql: include_str!("../../migrations/V20260303000002__notes.sql"),
    },
    Migration {
        version: 20260303000003,
        name: "task_state_fields",
        sql: include_str!("../../migrations/V20260303000003__task_state_fields.sql"),
    },
    Migration {
        version: 20260303000006,
        name: "model_health",
        sql: include_str!("../../migrations/V20260303000006__model_health.sql"),
    },
    Migration {
        version: 20260303000007,
        name: "note_links",
        sql: include_str!("../../migrations/V20260303000007__note_links.sql"),
    },
    Migration {
        version: 20260303000008,
        name: "task_memory_refs",
        sql: include_str!("../../migrations/V20260303000008__task_memory_refs.sql"),
    },
    Migration {
        version: 20260303000009,
        name: "credentials",
        sql: include_str!("../../migrations/V20260303000009__credentials.sql"),
    },
];

pub async fn run(pool: &SqlitePool) -> Result<()> {
    pool.execute(
        "CREATE TABLE IF NOT EXISTS refinery_schema_history (
             version INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             applied_on TEXT NOT NULL,
             checksum TEXT NOT NULL
         )",
    )
    .await?;

    for migration in MIGRATIONS {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM refinery_schema_history WHERE version = ?1",
        )
        .bind(migration.version)
        .fetch_one(pool)
        .await?;
        if exists > 0 {
            continue;
        }

        sqlx::raw_sql(migration.sql).execute(pool).await?;
        sqlx::query(
            "INSERT INTO refinery_schema_history (version, name, applied_on, checksum)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3)",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind("")
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
