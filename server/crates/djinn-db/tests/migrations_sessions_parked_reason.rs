use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

fn base_database_url() -> String {
    djinn_db::test_database_base_url()
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .map(|entry| {
            let path = entry.expect("migration dir entry").path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let version = name
                .split_once('_')
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| path.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    entries.sort_by(|(av, ap), (bv, bp)| {
        av.cmp(bv).then_with(|| {
            ap.file_name()
                .unwrap_or_default()
                .cmp(bp.file_name().unwrap_or_default())
        })
    });
    entries
}

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let db_name = format!(
        "djinn_migration_{}_{}",
        suffix,
        uuid::Uuid::now_v7().simple()
    );
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect postgres admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create migration test database");
    drop(admin);

    let db_url = format!("{prefix}/{db_name}");
    let result = f(db_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{db_name}""#).as_str())
        .await
        .expect("drop migration test database");

    result
}

async fn assert_parked_reason_schema(db_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db_url)
        .await
        .expect("connect migration test database");

    let column: Option<String> = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns WHERE table_name = 'sessions' AND column_name = 'parked_reason'",
    )
    .fetch_optional(&pool)
    .await
    .expect("inspect sessions.parked_reason column");
    assert_eq!(column.as_deref(), Some("text"));

    let nullable: Option<String> = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns WHERE table_name = 'sessions' AND column_name = 'parked_reason'",
    )
    .fetch_optional(&pool)
    .await
    .expect("inspect sessions.parked_reason nullability");
    assert_eq!(nullable.as_deref(), Some("YES"));

    let index_predicate: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'idx_sessions_parked_reason'",
    )
    .fetch_optional(&pool)
    .await
    .expect("inspect sessions.parked_reason index");
    assert_eq!(
        index_predicate.as_deref(),
        Some("(parked_reason IS NOT NULL)")
    );

    pool.close().await;
}

#[tokio::test]
async fn migration_59_applies_on_fresh_database() {
    with_temp_database("fresh", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;
        pool.close().await;

        assert_parked_reason_schema(&db_url).await;
    })
    .await;
}

#[tokio::test]
async fn migration_59_applies_after_prior_migrations() {
    with_temp_database("prior", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        for (version, path) in migration_entries(&migrations_dir()) {
            if version > 59 {
                break;
            }
            if version == 0 || version == 59 {
                continue;
            }
            let sql = std::fs::read_to_string(&path).expect("read migration sql");
            conn.execute(sql.as_str())
                .await
                .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
        }

        let migration_59 = migrations_dir().join("59_sessions_parked_reason.sql");
        let sql = std::fs::read_to_string(&migration_59).expect("read migration 59 sql");
        conn.execute(sql.as_str())
            .await
            .expect("apply migration 59 after prior migrations");
        drop(conn);

        assert_parked_reason_schema(&db_url).await;
    })
    .await;
}
