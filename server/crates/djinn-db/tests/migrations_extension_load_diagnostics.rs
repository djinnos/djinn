//! Migration 109 — `extension_load_diagnostics` table (epic wvg5 / proposal 0h1s).
//!
//! Verifies the new migration applies cleanly on a fresh database and on top of
//! the prior schema, and that the resulting schema carries the V1 columns,
//! CHECK constraints, foreign-key actions, and indexes the repository relies on.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 109;
const MIGRATION_FILE: &str = "109_extension_load_diagnostics.sql";

fn base_database_url() -> String {
    std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .unwrap_or_else(|_| {
            "postgres://djinn:VipjO1uAdxAGvNSA6EcJdZMdHAquYeJj@djinn-postgres.djinn.svc.cluster.local:5432/djinn"
                .to_owned()
        })
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

async fn apply_migration_109(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 109 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 109 after prior migrations");
}

async fn assert_schema(pool: &sqlx::PgPool) {
    // Table exists.
    let table: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'extension_load_diagnostics'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect extension_load_diagnostics table");
    assert_eq!(table.as_deref(), Some("extension_load_diagnostics"));

    // Required columns exist with the expected types and nullability.
    for (col, ty, nullable) in [
        ("id", "character varying", "NO"),
        ("project_id", "character varying", "NO"),
        ("task_id", "character varying", "YES"),
        ("session_id", "character varying", "YES"),
        ("load_attempt_id", "character varying", "NO"),
        ("schema_version", "smallint", "NO"),
        ("source_kind", "character varying", "NO"),
        ("source_key", "character varying", "NO"),
        ("phase", "character varying", "NO"),
        ("severity", "character varying", "NO"),
        ("summary", "character varying", "NO"),
        ("summary_fingerprint", "character varying", "NO"),
        ("remedy_code", "character varying", "NO"),
        ("remedy", "character varying", "NO"),
        ("occurrence_count", "integer", "NO"),
        ("first_seen_at", "character varying", "NO"),
        ("last_seen_at", "character varying", "NO"),
        ("created_at", "character varying", "NO"),
    ] {
        let (data_type, is_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'extension_load_diagnostics' AND column_name = $1",
        )
        .bind(col)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect column {col}: {e}"));
        assert_eq!(
            data_type.as_deref(),
            Some(ty),
            "extension_load_diagnostics.{col} should be {ty}, got {:?}",
            data_type
        );
        assert_eq!(
            is_nullable.as_deref(),
            Some(nullable),
            "extension_load_diagnostics.{col} nullability should be {nullable}, got {:?}",
            is_nullable
        );
    }

    // schema_version defaults to 1.
    let schema_version_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'extension_load_diagnostics' AND column_name = 'schema_version'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect schema_version default");
    let default = schema_version_default.unwrap_or_default();
    assert!(
        default.contains('1'),
        "schema_version default should contain 1, got: {default}"
    );

    // All expected CHECK constraints are present.
    let check_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint \
         WHERE conrelid = 'extension_load_diagnostics'::regclass \
           AND contype = 'c' \
         ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect extension_load_diagnostics CHECK constraints");
    for expected in [
        "chk_extension_load_diagnostics_schema_version",
        "chk_extension_load_diagnostics_association",
        "chk_extension_load_diagnostics_severity",
        "chk_extension_load_diagnostics_source_kind",
        "chk_extension_load_diagnostics_phase",
        "chk_extension_load_diagnostics_remedy_code",
        "chk_extension_load_diagnostics_occurrence_count",
    ] {
        assert!(
            check_constraints.iter().any(|c| c == expected),
            "expected CHECK constraint {expected}, got {check_constraints:?}"
        );
    }

    // Constraint bodies contain the expected vocabulary.
    for (conname, expected_values) in [
        ("chk_extension_load_diagnostics_schema_version", vec!["1"]),
        (
            "chk_extension_load_diagnostics_severity",
            vec!["warning", "error"],
        ),
        (
            "chk_extension_load_diagnostics_source_kind",
            vec!["project_mcp", "project_skill"],
        ),
        (
            "chk_extension_load_diagnostics_phase",
            vec![
                "placeholder_resolution",
                "process_start",
                "transport",
                "handshake",
                "tools_list",
                "frontmatter",
                "missing_file",
                "manifest_drift",
            ],
        ),
        (
            "chk_extension_load_diagnostics_remedy_code",
            vec![
                "check_placeholder",
                "check_command",
                "check_transport",
                "check_server",
                "check_skill_frontmatter",
                "restore_skill_file",
                "update_skill_manifest",
            ],
        ),
    ] {
        let body: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1",
        )
        .bind(conname)
        .fetch_optional(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect {conname}: {e}"));
        let body = body.unwrap_or_default();
        for value in expected_values {
            assert!(
                body.contains(&format!("'{value}'")),
                "{conname} should accept '{value}', got: {body}"
            );
        }
    }

    // Foreign keys exist with the lifecycle actions required by the epic:
    // project deletion cascades; task deletion clears the optional task_id;
    // session deletion cascades for session-associated rows.
    let mut fk_actions: Vec<(String, String)> = sqlx::query_as(
        "SELECT conname, \
            CASE confdeltype \
                WHEN 'a' THEN 'NO ACTION' \
                WHEN 'r' THEN 'RESTRICT' \
                WHEN 'c' THEN 'CASCADE' \
                WHEN 'n' THEN 'SET NULL' \
                WHEN 'd' THEN 'SET DEFAULT' \
            END as action \
         FROM pg_constraint \
         WHERE conrelid = 'extension_load_diagnostics'::regclass \
           AND contype = 'f' \
         ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect extension_load_diagnostics FKs");
    fk_actions.sort();
    let mut expected_fks = vec![
        (
            "fk_extension_load_diagnostics_project".to_owned(),
            "CASCADE".to_owned(),
        ),
        (
            "fk_extension_load_diagnostics_task".to_owned(),
            "SET NULL".to_owned(),
        ),
        (
            "fk_extension_load_diagnostics_session".to_owned(),
            "CASCADE".to_owned(),
        ),
    ];
    expected_fks.sort();
    assert_eq!(fk_actions, expected_fks, "FK actions do not match");

    // All expected indexes are present.
    let mut index_names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_index i ON i.indexrelid = c.oid \
         JOIN pg_class t ON t.oid = i.indrelid \
         WHERE t.relname = 'extension_load_diagnostics' \
           AND c.relname IN ( \
             'extension_load_diagnostics_pkey', \
             'idx_extension_load_diagnostics_project_id', \
             'idx_extension_load_diagnostics_session_id', \
             'idx_extension_load_diagnostics_task_id', \
             'idx_extension_load_diagnostics_load_attempt_id', \
             'idx_extension_load_diagnostics_order', \
             'uq_extension_load_diagnostics_dedupe' \
           )",
    )
    .fetch_all(pool)
    .await
    .expect("inspect extension_load_diagnostics indexes");
    index_names.sort();
    let mut expected_indexes = vec![
        "extension_load_diagnostics_pkey",
        "idx_extension_load_diagnostics_project_id",
        "idx_extension_load_diagnostics_session_id",
        "idx_extension_load_diagnostics_task_id",
        "idx_extension_load_diagnostics_load_attempt_id",
        "idx_extension_load_diagnostics_order",
        "uq_extension_load_diagnostics_dedupe",
    ];
    expected_indexes.sort();
    assert_eq!(index_names, expected_indexes, "indexes do not match");

    // Dedupe unique index treats NULLs as not distinct (Postgres 15+).
    let nulls_not_distinct: Option<bool> = sqlx::query_scalar(
        "SELECT i.indnullsnotdistinct \
         FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname = 'uq_extension_load_diagnostics_dedupe'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect dedupe unique index nulls treatment");
    assert_eq!(
        nulls_not_distinct,
        Some(true),
        "dedupe unique index must treat NULLs as not distinct"
    );
}

async fn assert_extension_load_diagnostics_schema(db_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db_url)
        .await
        .expect("connect migration test database");
    assert_schema(&pool).await;
    pool.close().await;
}

#[tokio::test]
async fn migration_109_applies_on_fresh_database() {
    with_temp_database("fresh_extension_load", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        sqlx::migrate!("./migrations_postgres")
            .run(&pool)
            .await
            .expect("apply all migrations to fresh database");
        pool.close().await;

        assert_extension_load_diagnostics_schema(&db_url).await;
    })
    .await;
}

#[tokio::test]
async fn migration_109_applies_after_prior_migrations() {
    with_temp_database("prior_extension_load", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;
        apply_migration_109(&mut conn).await;
        drop(conn);

        assert_extension_load_diagnostics_schema(&db_url).await;
    })
    .await;
}
