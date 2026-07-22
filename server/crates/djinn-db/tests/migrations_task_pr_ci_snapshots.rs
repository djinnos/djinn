use std::path::{Path, PathBuf};

use serde_json::Value;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 85;
const MIGRATION_FILE: &str = "85_task_pr_ci_snapshots.sql";

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

async fn apply_migrations_before_85(conn: &mut PgConnection) {
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

async fn apply_migration_85(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 85 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 85 after prior migrations");
}

async fn seed_project(conn: &mut PgConnection) {
    conn.execute(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('project-1', 'project-1', 'djinnos', 'djinn')",
    )
    .await
    .expect("seed project");
}

async fn seed_task(
    conn: &mut PgConnection,
    id: &str,
    short_id: &str,
    status: &str,
    pr_url: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, issue_type, status, \
          labels, acceptance_criteria, memory_refs, pr_url) \
         VALUES ($1, 'project-1', $2, 'title', 'description', 'design', 'task', $3, \
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $4)",
    )
    .bind(id)
    .bind(short_id)
    .bind(status)
    .bind(pr_url)
    .execute(&mut *conn)
    .await
    .expect("seed task");
}

async fn assert_schema(pool: &sqlx::PgPool) {
    let table: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'task_pr_ci_snapshots'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect task_pr_ci_snapshots table");
    assert_eq!(table.as_deref(), Some("task_pr_ci_snapshots"));

    for (column, data_type, nullable, default_contains) in [
        ("task_id", "character varying", "NO", None),
        ("pr_number", "bigint", "NO", None),
        ("head_sha", "character varying", "YES", None),
        ("ci_status", "text", "NO", Some("'unknown'::text")),
        (
            "blocking_required_check_names",
            "jsonb",
            "NO",
            Some("'[]'::jsonb"),
        ),
        ("failure_fingerprint", "text", "YES", None),
        ("first_seen_at", "character varying", "NO", Some("to_char")),
        ("last_seen_at", "character varying", "NO", Some("to_char")),
        ("same_signature_count", "bigint", "NO", Some("0")),
        (
            "last_remediation_base_sha",
            "character varying",
            "YES",
            None,
        ),
    ] {
        let (actual_type, actual_nullable, actual_default): (String, String, Option<String>) =
            sqlx::query_as(
                "SELECT data_type, is_nullable, column_default \
                 FROM information_schema.columns \
                 WHERE table_name = 'task_pr_ci_snapshots' AND column_name = $1",
            )
            .bind(column)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| panic!("inspect column {column}: {e}"));
        assert_eq!(actual_type, data_type, "unexpected type for {column}");
        assert_eq!(
            actual_nullable, nullable,
            "unexpected nullability for {column}"
        );
        if let Some(expected) = default_contains {
            assert!(
                actual_default
                    .as_deref()
                    .is_some_and(|d| d.contains(expected)),
                "default for {column} should contain {expected}, got {actual_default:?}"
            );
        }
    }

    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint \
         WHERE conrelid = 'task_pr_ci_snapshots'::regclass \
         ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect task_pr_ci_snapshots constraints");
    for expected in [
        "task_pr_ci_snapshots_pkey",
        "task_pr_ci_snapshots_task_id_fkey",
        "task_pr_ci_snapshots_ci_status_check",
        "task_pr_ci_snapshots_blocking_names_array_check",
        "task_pr_ci_snapshots_same_signature_count_check",
    ] {
        assert!(
            constraints.iter().any(|c| c == expected),
            "expected constraint {expected}, got {constraints:?}"
        );
    }
}

#[tokio::test]
async fn migration_85_applies_on_fresh_database_and_sets_defaults() {
    with_temp_database("fresh_ci", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        let operator =
            djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;

        assert_schema(&pool).await;

        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) \
             VALUES ('project-defaults', 'project-defaults', 'djinnos', 'djinn-defaults')",
        )
        .execute(&pool)
        .await
        .expect("seed defaults project");
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ('task-defaults', 'project-defaults', 'defaults', 'title', 'description', 'design', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $1)",
        )
        .bind(&operator)
        .execute(&pool)
        .await
        .expect("seed defaults task");
        sqlx::query("INSERT INTO task_pr_ci_snapshots (task_id, pr_number) VALUES ('task-defaults', 99)")
            .execute(&pool)
            .await
            .expect("insert default snapshot");

        type DefaultSnapshotRow = (
            Option<String>,
            String,
            Value,
            Option<String>,
            String,
            String,
            i64,
            Option<String>,
        );
        let row: DefaultSnapshotRow = sqlx::query_as(
                "SELECT head_sha, ci_status, blocking_required_check_names, failure_fingerprint, \
                        first_seen_at, last_seen_at, same_signature_count, last_remediation_base_sha \
                 FROM task_pr_ci_snapshots WHERE task_id = 'task-defaults' AND pr_number = 99",
            )
            .fetch_one(&pool)
            .await
            .expect("load default snapshot");
        assert_eq!(row.0, None);
        assert_eq!(row.1, "unknown");
        assert_eq!(row.2, serde_json::json!([]));
        assert_eq!(row.3, None);
        assert!(!row.4.is_empty(), "first_seen_at should default to a timestamp");
        assert!(!row.5.is_empty(), "last_seen_at should default to a timestamp");
        assert_eq!(row.6, 0);
        assert_eq!(row.7, None);

        sqlx::query(
            "INSERT INTO task_pr_ci_snapshots (task_id, pr_number, ci_status) \
             VALUES ('task-defaults', 100, 'skipped')",
        )
        .execute(&pool)
        .await
        .expect_err("ci_status check should reject non-CiStatus wire values");

        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_85_backfills_open_pr_tasks_to_unknown_and_skips_closed_tasks() {
    with_temp_database("prior_ci", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_migrations_before_85(&mut conn).await;
        seed_project(&mut conn).await;
        seed_task(
            &mut conn,
            "open-pr-task",
            "open-pr",
            "pr_review",
            Some("https://github.com/djinnos/djinn/pull/42"),
        )
        .await;
        seed_task(
            &mut conn,
            "fragment-pr-task",
            "frag-pr",
            "approved",
            Some("https://github.com/djinnos/djinn/pull/43#discussion"),
        )
        .await;
        seed_task(
            &mut conn,
            "closed-pr-task",
            "closed-pr",
            "closed",
            Some("https://github.com/djinnos/djinn/pull/44"),
        )
        .await;
        seed_task(&mut conn, "no-pr-task", "no-pr", "open", None).await;

        apply_migration_85(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");
        assert_schema(&pool).await;

        type BackfilledSnapshotRow = (
            String,
            i64,
            Option<String>,
            String,
            Value,
            Option<String>,
            String,
            String,
            i64,
            Option<String>,
        );
        let rows: Vec<BackfilledSnapshotRow> = sqlx::query_as(
            "SELECT task_id, pr_number, head_sha, ci_status, blocking_required_check_names, \
                        failure_fingerprint, first_seen_at, last_seen_at, same_signature_count, \
                        last_remediation_base_sha \
                 FROM task_pr_ci_snapshots ORDER BY task_id",
        )
        .fetch_all(&pool)
        .await
        .expect("load backfilled snapshots");

        assert_eq!(
            rows.len(),
            2,
            "only non-closed PR-backed tasks are backfilled"
        );
        assert_eq!(rows[0].0, "fragment-pr-task");
        assert_eq!(rows[0].1, 43);
        assert_eq!(rows[1].0, "open-pr-task");
        assert_eq!(rows[1].1, 42);

        for row in rows {
            assert_eq!(row.2, None, "unknown backfill leaves head_sha null");
            assert_eq!(row.3, "unknown");
            assert_eq!(row.4, serde_json::json!([]));
            assert_eq!(row.5, None, "unknown backfill leaves fingerprint null");
            assert!(!row.6.is_empty(), "first_seen_at should be populated");
            assert!(!row.7.is_empty(), "last_seen_at should be populated");
            assert_eq!(row.8, 0);
            assert_eq!(row.9, None, "unknown backfill leaves remediation base null");
        }

        let closed_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM task_pr_ci_snapshots WHERE task_id = 'closed-pr-task'",
        )
        .fetch_one(&pool)
        .await
        .expect("count closed task snapshots");
        assert_eq!(
            closed_count, 0,
            "closed tasks are not retroactively backfilled"
        );

        pool.close().await;
    })
    .await;
}

/// Migration 116 is purely additive: it adds the nullable `mq_*` merge-queue
/// lane columns plus two CHECK constraints, with no backfill. Assert the
/// columns exist as nullable-without-default, the constraints are present, and
/// that they reject bad values while accepting a NULL lane and a well-formed
/// lane.
#[tokio::test]
async fn migration_116_adds_nullable_merge_queue_lane_columns_and_constraints() {
    with_temp_database("mq_lane", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        let operator =
            djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;

        // Pre-existing PR-head lane schema is still intact.
        assert_schema(&pool).await;

        for (column, data_type) in [
            ("mq_state", "text"),
            ("mq_run_id", "bigint"),
            ("mq_head_sha", "character varying"),
            ("mq_failed_check_names", "jsonb"),
            ("mq_failure_fingerprint", "text"),
            ("mq_same_signature_count", "bigint"),
            ("mq_first_seen_at", "character varying"),
            ("mq_last_seen_at", "character varying"),
        ] {
            let (actual_type, actual_nullable, actual_default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default \
                     FROM information_schema.columns \
                     WHERE table_name = 'task_pr_ci_snapshots' AND column_name = $1",
                )
                .bind(column)
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("inspect mq column {column}: {e}"));
            assert_eq!(actual_type, data_type, "unexpected type for {column}");
            assert_eq!(actual_nullable, "YES", "mq column {column} must be nullable");
            assert_eq!(
                actual_default, None,
                "mq column {column} must have no default"
            );
        }

        let constraints: Vec<String> = sqlx::query_scalar(
            "SELECT conname FROM pg_constraint \
             WHERE conrelid = 'task_pr_ci_snapshots'::regclass \
             ORDER BY conname",
        )
        .fetch_all(&pool)
        .await
        .expect("inspect task_pr_ci_snapshots constraints");
        for expected in [
            "task_pr_ci_snapshots_mq_failed_names_array_check",
            "task_pr_ci_snapshots_mq_same_signature_count_check",
        ] {
            assert!(
                constraints.iter().any(|c| c == expected),
                "expected constraint {expected}, got {constraints:?}"
            );
        }

        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) \
             VALUES ('project-mq', 'project-mq', 'djinnos', 'djinn-mq')",
        )
        .execute(&pool)
        .await
        .expect("seed mq project");
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ('task-mq', 'project-mq', 'mq', 'title', 'description', 'design', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $1)",
        )
        .bind(&operator)
        .execute(&pool)
        .await
        .expect("seed mq task");

        // A NULL merge-queue lane is allowed (the additive default).
        sqlx::query("INSERT INTO task_pr_ci_snapshots (task_id, pr_number) VALUES ('task-mq', 1)")
            .execute(&pool)
            .await
            .expect("NULL mq lane is permitted");

        // A well-formed lane (JSON array + non-negative count) is accepted.
        sqlx::query(
            "INSERT INTO task_pr_ci_snapshots \
             (task_id, pr_number, mq_state, mq_failed_check_names, mq_same_signature_count) \
             VALUES ('task-mq', 2, 'dequeued_failure', '[\"Server Test\"]'::jsonb, 1)",
        )
        .execute(&pool)
        .await
        .expect("well-formed mq lane is accepted");

        // A non-array mq_failed_check_names is rejected by the CHECK.
        sqlx::query(
            "INSERT INTO task_pr_ci_snapshots \
             (task_id, pr_number, mq_failed_check_names) \
             VALUES ('task-mq', 3, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect_err("non-array mq_failed_check_names should be rejected");

        // A negative mq_same_signature_count is rejected by the CHECK.
        sqlx::query(
            "INSERT INTO task_pr_ci_snapshots \
             (task_id, pr_number, mq_same_signature_count) \
             VALUES ('task-mq', 4, -1)",
        )
        .execute(&pool)
        .await
        .expect_err("negative mq_same_signature_count should be rejected");

        pool.close().await;
    })
    .await;
}
