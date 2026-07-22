//! Migration 61 — `doctor_findings` table (Doctor framework epic 08f0).
//!
//! Verifies the new migration applies cleanly on a fresh database AND on
//! top of the prior schema (so additive ordering is correct), and that the
//! resulting schema carries the columns, CHECK constraint, and indexes the
//! repository relies on.
//!
//! Mirrors the pattern in `migrations_sessions_parked_reason.rs` (the
//! previous migration test) so the new test fits the existing harness
//! without inventing a new test infra.

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

async fn assert_doctor_findings_schema(db_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db_url)
        .await
        .expect("connect migration test database");

    // Table exists.
    let table: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'doctor_findings'",
    )
    .fetch_optional(&pool)
    .await
    .expect("inspect doctor_findings table");
    assert_eq!(table.as_deref(), Some("doctor_findings"));

    // Required columns exist with the expected types / nullability.
    for (col, ty, nullable) in [
        ("id", "character varying", "NO"),
        ("run_id", "character varying", "YES"),
        ("created_at", "character varying", "NO"),
        ("check_name", "character varying", "NO"),
        ("severity", "character varying", "NO"),
        ("entity_ids", "jsonb", "NO"),
        ("evidence", "jsonb", "NO"),
        ("resolver_snapshot", "jsonb", "YES"),
        ("detail", "text", "YES"),
    ] {
        let (data_type, is_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'doctor_findings' AND column_name = $1",
        )
        .bind(col)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("inspect column {col}: {e}"));
        assert_eq!(
            data_type.as_deref(),
            Some(ty),
            "doctor_findings.{col} should be {ty}, got {:?}",
            data_type
        );
        assert_eq!(
            is_nullable.as_deref(),
            Some(nullable),
            "doctor_findings.{col} nullability should be {nullable}, got {:?}",
            is_nullable
        );
    }

    // Severity CHECK constraint is in place.
    let constraint: Option<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint \
         WHERE conrelid = 'doctor_findings'::regclass \
           AND contype = 'c' AND conname = 'doctor_findings_severity_check'",
    )
    .fetch_optional(&pool)
    .await
    .expect("inspect severity CHECK constraint");
    assert_eq!(
        constraint.as_deref(),
        Some("doctor_findings_severity_check"),
        "expected doctor_findings_severity_check constraint to be present"
    );

    // Required indexes are in place.
    for index_name in [
        "doctor_findings_pkey",
        "doctor_findings_created_at_idx",
        "doctor_findings_check_name_idx",
        "doctor_findings_check_name_created_at_idx",
        "doctor_findings_entity_ids_gin_idx",
    ] {
        let present: Option<String> = sqlx::query_scalar(
            "SELECT c.relname FROM pg_class c \
             JOIN pg_index i ON i.indexrelid = c.oid \
             JOIN pg_class t ON t.oid = i.indrelid \
             WHERE t.relname = 'doctor_findings' AND c.relname = $1",
        )
        .bind(index_name)
        .fetch_optional(&pool)
        .await
        .unwrap_or_else(|e| panic!("inspect index {index_name}: {e}"));
        assert_eq!(
            present.as_deref(),
            Some(index_name),
            "expected index {index_name} on doctor_findings"
        );
    }

    pool.close().await;
}

#[tokio::test]
async fn migration_61_applies_on_fresh_database() {
    with_temp_database("fresh_doctor", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;
        pool.close().await;

        assert_doctor_findings_schema(&db_url).await;
    })
    .await;
}

#[tokio::test]
async fn migration_61_applies_after_prior_migrations() {
    with_temp_database("prior_doctor", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        for (version, path) in migration_entries(&migrations_dir()) {
            if version > 61 {
                break;
            }
            if version == 0 || version == 61 {
                continue;
            }
            let sql = std::fs::read_to_string(&path).expect("read migration sql");
            conn.execute(sql.as_str())
                .await
                .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
        }

        let migration_61 = migrations_dir().join("61_doctor_findings.sql");
        let sql = std::fs::read_to_string(&migration_61).expect("read migration 61 sql");
        conn.execute(sql.as_str())
            .await
            .expect("apply migration 61 after prior migrations");
        drop(conn);

        assert_doctor_findings_schema(&db_url).await;
    })
    .await;
}
