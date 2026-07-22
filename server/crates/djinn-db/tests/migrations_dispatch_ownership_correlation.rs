//! Migration 133 — `coordinator_incarnations` table + nullable
//! `dispatch_owner_incarnation_id` / `dispatch_group_id` correlation columns on
//! `task_attempts` and `dispatch_group_id` on `task_runs` (epic jy7g / proposal
//! 9gg5).
//!
//! Verifies the migration applies cleanly on a fresh database AND on top of the
//! prior schema (V1..V132), and that pre-existing task_attempts and task_runs
//! survive the additive migration with NULL owner/group identifiers — no
//! heuristic backfill occurs.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 133;
const MIGRATION_FILE: &str = "133_dispatch_ownership_correlation.sql";

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

/// Apply every migration whose version prefix is strictly less than
/// `MIGRATION_VERSION`.
async fn apply_prior_migrations(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        if version == 0 {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration sql");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
    }
}

async fn apply_migration_133(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 133 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 133 after prior migrations");
}

/// Assert the schema additions from migration 133 are present.
async fn assert_dispatch_ownership_schema(pool: &sqlx::PgPool) {
    // ── coordinator_incarnations table ───────────────────────────────────
    let table: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'coordinator_incarnations'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect coordinator_incarnations table");
    assert_eq!(
        table.as_deref(),
        Some("coordinator_incarnations"),
        "coordinator_incarnations table should exist"
    );

    for (column, data_type, nullable) in [
        ("id", "character varying", "NO"),
        ("registered_at", "character varying", "NO"),
        ("last_renewed_at", "character varying", "NO"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'coordinator_incarnations' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect coordinator_incarnations.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "coordinator_incarnations.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some(nullable),
            "coordinator_incarnations.{column} nullability should be {nullable}, got {actual_nullable:?}"
        );
    }

    // ── task_attempts: new columns ───────────────────────────────────────
    for (column, data_type, nullable) in [
        ("dispatch_owner_incarnation_id", "character varying", "YES"),
        ("dispatch_group_id", "character varying", "YES"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'task_attempts' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect task_attempts.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "task_attempts.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some(nullable),
            "task_attempts.{column} nullability should be {nullable}, got {actual_nullable:?}"
        );
    }

    // ── task_runs: new column ────────────────────────────────────────────
    let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable \
         FROM information_schema.columns \
         WHERE table_name = 'task_runs' AND column_name = 'dispatch_group_id'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("inspect task_runs.dispatch_group_id: {e}"));
    assert_eq!(
        actual_type.as_deref(),
        Some("character varying"),
        "task_runs.dispatch_group_id should be character varying"
    );
    assert_eq!(
        actual_nullable.as_deref(),
        Some("YES"),
        "task_runs.dispatch_group_id should be nullable"
    );

    // ── indexes ──────────────────────────────────────────────────────────
    let index_names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_index i ON i.indexrelid = c.oid \
         JOIN pg_class t ON t.oid = i.indrelid \
         WHERE t.relname IN ('task_attempts', 'task_runs') \
           AND c.relname IN ( \
             'idx_task_attempts_dispatch_owner_incarnation', \
             'idx_task_attempts_dispatch_group', \
             'idx_task_runs_dispatch_group' \
           ) \
         ORDER BY c.relname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect dispatch ownership indexes");
    let mut expected = vec![
        "idx_task_attempts_dispatch_group",
        "idx_task_attempts_dispatch_owner_incarnation",
        "idx_task_runs_dispatch_group",
    ];
    expected.sort();
    assert_eq!(
        index_names, expected,
        "expected dispatch ownership indexes, got {index_names:?}"
    );

    // The partial index predicates.
    let owner_pred: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'idx_task_attempts_dispatch_owner_incarnation'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect owner index predicate");
    assert_eq!(
        owner_pred.as_deref(),
        Some("(dispatch_owner_incarnation_id IS NOT NULL)"),
        "owner index predicate mismatch: {owner_pred:?}"
    );

    let group_pred: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'idx_task_attempts_dispatch_group'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect group index predicate");
    assert!(
        group_pred
            .as_deref()
            .unwrap_or("")
            .contains("dispatch_group_id IS NOT NULL"),
        "group index predicate should include dispatch_group_id IS NOT NULL, got {group_pred:?}"
    );
    let group_pred = group_pred.as_deref().unwrap_or("");
    assert!(
        group_pred.contains("outcome IN") || group_pred.contains("outcome)::text = ANY"),
        "group index predicate should restrict outcomes to pending/submitted, got {group_pred:?}"
    );
    assert!(
        group_pred.contains("pending") && group_pred.contains("submitted"),
        "group index predicate should include pending and submitted outcomes, got {group_pred:?}"
    );
}

/// Seed a project / task / task_attempt / task_run so we can prove that old
/// rows from a pre-migration-133 database stay readable after the migration is
/// applied. This is the "old rows remain readable without backfill" invariant.
async fn seed_legacy_rows(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('project-131', 'project-131', 'djinnos', 'djinn-131')",
    )
    .execute(pool)
    .await
    .expect("seed legacy project");

    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) \
         VALUES ('user-131', 9000000131, 'legacy-creator-131') ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed legacy creator");

    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
         VALUES ('task-131', 'project-131', 't131', 'title', 'description', 'design', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, 'user-131')",
    )
    .execute(pool)
    .await
    .expect("seed legacy task");

    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
         VALUES ('run-131', 'project-131', 'task-131', 'manual', 'pending')",
    )
    .execute(pool)
    .await
    .expect("seed legacy task_run");

    sqlx::query(
        "INSERT INTO task_attempts (id, task_id, role, attempt_seq, dispatch_key, outcome) \
         VALUES ('attempt-131', 'task-131', 'worker', 1, 'dk-131', 'pending')",
    )
    .execute(pool)
    .await
    .expect("seed legacy task_attempt");
}

/// Assert legacy rows are readable and their new columns are NULL — no
/// heuristic backfill occurred.
async fn assert_legacy_rows_null_and_readable(pool: &sqlx::PgPool) {
    let (owner, group): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT dispatch_owner_incarnation_id, dispatch_group_id \
         FROM task_attempts WHERE id = 'attempt-131'",
    )
    .fetch_one(pool)
    .await
    .expect("load legacy task_attempt");
    assert_eq!(
        owner, None,
        "legacy task_attempt dispatch_owner_incarnation_id should be NULL"
    );
    assert_eq!(
        group, None,
        "legacy task_attempt dispatch_group_id should be NULL"
    );

    let (group,): (Option<String>,) = sqlx::query_as(
        "SELECT dispatch_group_id \
         FROM task_runs WHERE id = 'run-131'",
    )
    .fetch_one(pool)
    .await
    .expect("load legacy task_run");
    assert_eq!(
        group, None,
        "legacy task_run dispatch_group_id should be NULL"
    );
}

#[tokio::test]
async fn migration_133_applies_on_fresh_database() {
    with_temp_database("fresh_dispatch_ownership", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;

        assert_dispatch_ownership_schema(&pool).await;
        seed_legacy_rows(&pool).await;
        assert_legacy_rows_null_and_readable(&pool).await;

        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_133_applies_after_prior_migrations_and_preserves_legacy_rows() {
    with_temp_database("prior_dispatch_ownership", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;

        // Seed legacy rows BEFORE migration 133 applies so we can prove the
        // migration does not require backfill for old task_attempts/task_runs.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect prior migration database (pool)");
        seed_legacy_rows(&pool).await;
        pool.close().await;

        apply_migration_133(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");

        assert_dispatch_ownership_schema(&pool).await;
        assert_legacy_rows_null_and_readable(&pool).await;

        pool.close().await;
    })
    .await;
}
