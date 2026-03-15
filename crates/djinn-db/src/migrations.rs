use sqlx::SqlitePool;

mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations");
}

pub async fn run(pool: &SqlitePool) -> Result<(), sqlx::Error> {
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
            let clean_sql = strip_pragmas(sql);
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

async fn run_migration_sql(pool: &SqlitePool, sql: &str) -> Result<(), sqlx::Error> {
    let pool = pool.clone();
    let sql = sql.to_owned();
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(async {
            let mut conn = pool.acquire().await?;
            sqlx::raw_sql("PRAGMA foreign_keys = OFF")
                .execute(&mut *conn)
                .await?;
            sqlx::raw_sql("BEGIN EXCLUSIVE").execute(&mut *conn).await?;

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
                    Err(e)
                }
            }
        })
    })
    .await
    .expect("migration task panicked")
}

fn strip_pragmas(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim().to_uppercase().starts_with("PRAGMA "))
        .collect::<Vec<_>>()
        .join("\n")
}
