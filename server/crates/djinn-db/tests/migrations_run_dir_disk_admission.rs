//! Migration 151 — `run_dirs` durable ledger for disk-aware build admission
//! (proposal nquz, phase 1).
//!
//! Verifies the migration applies cleanly on a fresh database AND on top of the
//! prior schema (so additive ordering is correct), installs the documented
//! columns / CHECK constraints / indexes, and — critically — does NOT modify any
//! existing `admission_journal` row.
//!
//! Mirrors the harness in `migrations_liveness_evidence_outcomes.rs`.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 151;
const MIGRATION_FILE: &str = "151_run_dir_disk_admission.sql";

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

/// Designated operator the creator-contract migration (142) validates against.
const MIGRATION_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000151";
/// The version of the creator-contract migration that requires the operator.
const CREATOR_CONTRACT_VERSION: u64 = 142;

/// Seed the validated designated operator required by migration 142.
async fn seed_migration_operator(conn: &mut PgConnection) {
    conn.execute(
        format!(
            "INSERT INTO users (id, github_id, github_login) \
             VALUES ('{MIGRATION_OPERATOR_ID}', 9000000151, 'run-dir-migration-operator') \
             ON CONFLICT DO NOTHING"
        )
        .as_str(),
    )
    .await
    .expect("seed designated migration operator");
}

async fn apply_prior_migrations(conn: &mut PgConnection) {
    // Migration 142 (task creator contract) refuses to run without a validated
    // designated operator supplied via a session GUC. Set the GUC for the whole
    // session, then seed the operator user just before that migration applies.
    conn.execute(
        format!(
            "SELECT set_config('djinn.migration_designated_operator_user_id', '{MIGRATION_OPERATOR_ID}', false)"
        )
        .as_str(),
    )
    .await
    .expect("set designated operator GUC");

    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        if version == 0 {
            continue;
        }
        if version == CREATOR_CONTRACT_VERSION {
            seed_migration_operator(conn).await;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration sql");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
    }
}

async fn apply_migration_151(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 151 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 151 after prior migrations");
}

async fn assert_run_dirs_schema(pool: &sqlx::PgPool) {
    for (column, data_type, nullable) in [
        ("volume_id", "character varying", "NO"),
        ("pod_uid", "character varying", "NO"),
        ("task_run_id", "character varying", "YES"),
        ("project_id", "character varying", "YES"),
        ("base_fingerprint", "character varying", "YES"),
        ("state", "character varying", "NO"),
        ("generation", "bigint", "NO"),
        ("reserved_bytes", "bigint", "NO"),
        ("measured_bytes", "bigint", "NO"),
        ("quota_id", "character varying", "YES"),
        ("last_lease_at", "timestamp with time zone", "YES"),
        ("temp_path", "text", "YES"),
        ("final_path", "text", "YES"),
        ("created_at", "timestamp with time zone", "NO"),
        ("updated_at", "timestamp with time zone", "NO"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable FROM information_schema.columns \
             WHERE table_name = 'run_dirs' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect run_dirs.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "run_dirs.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some(nullable),
            "run_dirs.{column} nullability should be {nullable}, got {actual_nullable:?}"
        );
    }

    // Primary key is (volume_id, pod_uid).
    let pk_cols: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname FROM pg_index i \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
         WHERE i.indrelid = 'run_dirs'::regclass AND i.indisprimary ORDER BY a.attname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect run_dirs primary key");
    assert_eq!(
        pk_cols,
        vec!["pod_uid".to_string(), "volume_id".to_string()]
    );

    // The state CHECK constraint accepts every documented state.
    let state_check: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'run_dirs_state_check'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect run_dirs_state_check body");
    let body = state_check.unwrap_or_default();
    for value in [
        "absent",
        "reserved",
        "seeding",
        "ready_active",
        "ready_idle",
        "reclaimable",
        "reclaiming",
        "quarantined_unowned",
    ] {
        assert!(
            body.contains(&format!("'{value}'")),
            "run_dirs_state_check should accept '{value}', got body: {body}"
        );
    }

    // Non-negative byte / generation constraints exist.
    let check_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint WHERE conrelid = 'run_dirs'::regclass \
           AND contype = 'c' ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect run_dirs CHECK constraints");
    for expected in [
        "run_dirs_generation_nonneg",
        "run_dirs_measured_bytes_nonneg",
        "run_dirs_reserved_bytes_nonneg",
        "run_dirs_state_check",
    ] {
        assert!(
            check_constraints.iter().any(|c| c == expected),
            "expected CHECK {expected} on run_dirs, got {check_constraints:?}"
        );
    }

    // Indexes.
    let index_names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_index i ON i.indexrelid = c.oid \
         JOIN pg_class t ON t.oid = i.indrelid \
         WHERE t.relname = 'run_dirs' AND c.relname IN ( \
             'run_dirs_volume_state_idx', 'run_dirs_task_run_idx', \
             'run_dirs_project_fingerprint_idx') ORDER BY c.relname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect run_dirs indexes");
    let mut expected_indexes = vec![
        "run_dirs_project_fingerprint_idx",
        "run_dirs_task_run_idx",
        "run_dirs_volume_state_idx",
    ];
    expected_indexes.sort();
    assert_eq!(index_names, expected_indexes);
}

/// Seed a live admission_journal row through raw SQL so we can prove migration
/// 151 leaves existing admission rows byte-for-byte unmodified.
async fn seed_admission_row(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO admission_journal \
         (domain, work_id, generation, workload_kind, state, creator_server_epoch, object_name) \
         VALUES ('task_observation', 'work-preexisting', 0, 'task', 'live', 'epoch-1', 'obj-1')",
    )
    .execute(pool)
    .await
    .expect("seed pre-existing admission row");
}

async fn assert_admission_row_unchanged(pool: &sqlx::PgPool) {
    let (state, epoch): (String, String) = sqlx::query_as(
        "SELECT state, creator_server_epoch FROM admission_journal WHERE work_id = 'work-preexisting'",
    )
    .fetch_one(pool)
    .await
    .expect("load pre-existing admission row");
    assert_eq!(
        state, "live",
        "migration must not modify existing admission rows"
    );
    assert_eq!(epoch, "epoch-1");
}

#[tokio::test]
async fn migration_151_applies_on_fresh_database() {
    with_temp_database("fresh_run_dirs", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;

        assert_run_dirs_schema(&pool).await;

        // A fresh run_dir row can be inserted and read back.
        sqlx::query(
            "INSERT INTO run_dirs (volume_id, pod_uid, state) VALUES ('vol', 'pod', 'reserved')",
        )
        .execute(&pool)
        .await
        .expect("insert run_dir row");
        let state: String =
            sqlx::query_scalar("SELECT state FROM run_dirs WHERE volume_id = 'vol'")
                .fetch_one(&pool)
                .await
                .expect("read run_dir row");
        assert_eq!(state, "reserved");

        // An out-of-vocabulary state is rejected by the CHECK constraint.
        let bad = sqlx::query(
            "INSERT INTO run_dirs (volume_id, pod_uid, state) VALUES ('vol', 'pod2', 'bogus')",
        )
        .execute(&pool)
        .await;
        assert!(bad.is_err(), "state CHECK must reject an unknown state");

        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_151_applies_after_prior_migrations_without_touching_admission() {
    with_temp_database("prior_run_dirs", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect prior migration database (pool)");
        seed_admission_row(&pool).await;
        pool.close().await;

        apply_migration_151(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");

        assert_run_dirs_schema(&pool).await;
        assert_admission_row_unchanged(&pool).await;

        pool.close().await;
    })
    .await;
}
