//! Migration 131 — retire manifest-specific `extension_load_diagnostics` values.
//!
//! Verifies the new migration applies cleanly on a fresh database and on top of
//! the prior schema, and that the resulting schema carries the V1 columns,
//! CHECK constraints, foreign-key actions, and indexes the repository relies on.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 131;
const MIGRATION_FILE: &str = "131_retire_skill_manifest_diagnostics.sql";

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

async fn apply_migration_131(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 131 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 131 after prior migrations");
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

    // The schema_version constraint is numeric and is not quoted in the definition.
    let schema_version_body: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1",
    )
    .bind("chk_extension_load_diagnostics_schema_version")
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| panic!("inspect schema_version constraint: {e}"));
    let schema_version_body = schema_version_body.unwrap_or_default();
    assert!(
        schema_version_body.contains("= 1"),
        "schema_version should be fixed to 1, got: {schema_version_body}"
    );

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

    // Project ownership trigger is installed so that optional task/session
    // associations cannot reference a task/session belonging to a different project.
    let trigger_present: Option<bool> = sqlx::query_scalar(
        "SELECT TRUE FROM pg_trigger \
         WHERE tgrelid = 'extension_load_diagnostics'::regclass \
           AND tgname = 'trg_extension_load_diagnostics_project_ownership'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect extension_load_diagnostics ownership trigger");
    assert_eq!(
        trigger_present,
        Some(true),
        "project ownership trigger must be present"
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
async fn migration_131_applies_on_fresh_database() {
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
async fn migration_131_applies_after_prior_migrations() {
    with_temp_database("prior_extension_load", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;
        apply_migration_131(&mut conn).await;
        drop(conn);

        assert_extension_load_diagnostics_schema(&db_url).await;
    })
    .await;
}

async fn insert_project(
    pool: &sqlx::PgPool,
    id: &str,
    name: &str,
    github_owner: &str,
    github_repo: &str,
) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(github_owner)
    .bind(github_repo)
    .execute(pool)
    .await
    .expect("insert project");
}

async fn insert_task(pool: &sqlx::PgPool, id: &str, project_id: &str, short_id: &str) {
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, issue_type, status, priority, owner, labels, acceptance_criteria, memory_refs) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
    )
    .bind(id)
    .bind(project_id)
    .bind(short_id)
    .bind("title")
    .bind("description")
    .bind("design")
    .bind("task")
    .bind("open")
    .bind(1)
    .bind("")
    .execute(pool)
    .await
    .expect("insert task");
}

async fn insert_session(pool: &sqlx::PgPool, id: &str, project_id: &str) {
    sqlx::query(
        "INSERT INTO sessions (id, project_id, model_id, agent_type, status) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(project_id)
    .bind("gpt-4")
    .bind("worker")
    .bind("active")
    .execute(pool)
    .await
    .expect("insert session");
}

async fn insert_diagnostic(
    pool: &sqlx::PgPool,
    id: &str,
    project_id: &str,
    task_id: Option<&str>,
    session_id: Option<&str>,
    attempt_id: &str,
) -> sqlx::Result<sqlx::postgres::PgQueryResult> {
    sqlx::query(
        "INSERT INTO extension_load_diagnostics \
            (id, project_id, task_id, session_id, load_attempt_id, source_kind, source_key, phase, \
             severity, summary, summary_fingerprint, remedy_code, remedy, occurrence_count, \
             first_seen_at, last_seen_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
    )
    .bind(id)
    .bind(project_id)
    .bind(task_id)
    .bind(session_id)
    .bind(attempt_id)
    .bind("project_mcp")
    .bind("mcp://test")
    .bind("handshake")
    .bind("error")
    .bind("summary")
    .bind("fingerprint")
    .bind("check_server")
    .bind("remedy text")
    .bind(1)
    .bind("2024-01-01T00:00:00Z")
    .bind("2024-01-01T00:00:00Z")
    .execute(pool)
    .await
}

#[tokio::test]
async fn migration_131_enforces_project_ownership_and_lifecycle() {
    with_temp_database("ownership_extension_load", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect ownership migration database");
        apply_prior_migrations(&mut conn).await;
        apply_migration_131(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect ownership migration database");

        let project_a = uuid::Uuid::now_v7().to_string();
        let project_b = uuid::Uuid::now_v7().to_string();
        insert_project(&pool, &project_a, "project-a", "owner-a", "repo-a").await;
        insert_project(&pool, &project_b, "project-b", "owner-b", "repo-b").await;

        let task_b = uuid::Uuid::now_v7().to_string();
        insert_task(&pool, &task_b, &project_b, "t1").await;
        let session_b = uuid::Uuid::now_v7().to_string();
        insert_session(&pool, &session_b, &project_b).await;
        let session_a = uuid::Uuid::now_v7().to_string();
        insert_session(&pool, &session_a, &project_a).await;

        // Both associations from project B used for a project A diagnostic is rejected.
        let diag_id = uuid::Uuid::now_v7().to_string();
        let attempt_id = uuid::Uuid::now_v7().to_string();
        let result = insert_diagnostic(
            &pool,
            &diag_id,
            &project_a,
            Some(&task_b),
            Some(&session_b),
            &attempt_id,
        )
        .await;
        assert!(
            result.is_err(),
            "expected cross-project (task+session) association to be rejected"
        );

        // A mismatched task association is rejected even when the session belongs to the diagnostic's project.
        let diag_id2 = uuid::Uuid::now_v7().to_string();
        let attempt_id2 = uuid::Uuid::now_v7().to_string();
        let result = insert_diagnostic(
            &pool,
            &diag_id2,
            &project_a,
            Some(&task_b),
            Some(&session_a),
            &attempt_id2,
        )
        .await;
        assert!(
            result.is_err(),
            "expected cross-project task association to be rejected"
        );

        // A mismatched session association is rejected even without a task.
        let diag_id3 = uuid::Uuid::now_v7().to_string();
        let attempt_id3 = uuid::Uuid::now_v7().to_string();
        let result = insert_diagnostic(
            &pool,
            &diag_id3,
            &project_a,
            None,
            Some(&session_b),
            &attempt_id3,
        )
        .await;
        assert!(
            result.is_err(),
            "expected cross-project session association to be rejected"
        );

        // Doctor-only and matching session-associated rows are accepted.
        let diag_id4 = uuid::Uuid::now_v7().to_string();
        let attempt_id4 = uuid::Uuid::now_v7().to_string();
        insert_diagnostic(&pool, &diag_id4, &project_a, None, None, &attempt_id4)
            .await
            .expect("doctor-only diagnostic should insert");

        let diag_id5 = uuid::Uuid::now_v7().to_string();
        let attempt_id5 = uuid::Uuid::now_v7().to_string();
        insert_diagnostic(
            &pool,
            &diag_id5,
            &project_a,
            None,
            Some(&session_a),
            &attempt_id5,
        )
        .await
        .expect("session-associated diagnostic should insert");

        // A matching task/session pair is accepted.
        let task_a = uuid::Uuid::now_v7().to_string();
        insert_task(&pool, &task_a, &project_a, "t2").await;
        let diag_id6 = uuid::Uuid::now_v7().to_string();
        let attempt_id6 = uuid::Uuid::now_v7().to_string();
        insert_diagnostic(
            &pool,
            &diag_id6,
            &project_a,
            Some(&task_a),
            Some(&session_a),
            &attempt_id6,
        )
        .await
        .expect("valid task+session diagnostic should insert");

        // Deleting the task clears task_id without removing the diagnostic.
        sqlx::query("DELETE FROM tasks WHERE id = $1")
            .bind(&task_a)
            .execute(&pool)
            .await
            .expect("delete task");
        let task_id_cleared: Option<String> =
            sqlx::query_scalar("SELECT task_id FROM extension_load_diagnostics WHERE id = $1")
                .bind(&diag_id6)
                .fetch_one(&pool)
                .await
                .expect("fetch task_id after task deletion");
        assert!(
            task_id_cleared.is_none(),
            "task_id should be set to NULL when the task is deleted"
        );

        // Owning session deletion cascades to the session-associated rows.
        sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(&session_a)
            .execute(&pool)
            .await
            .expect("delete session");
        let remaining_session: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM extension_load_diagnostics WHERE id = $1")
                .bind(&diag_id5)
                .fetch_one(&pool)
                .await
                .expect("count session-associated diagnostic");
        assert_eq!(
            remaining_session, 0,
            "session-associated diagnostic should be deleted"
        );
        let remaining_task: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM extension_load_diagnostics WHERE id = $1")
                .bind(&diag_id6)
                .fetch_one(&pool)
                .await
                .expect("count task+session diagnostic");
        assert_eq!(
            remaining_task, 0,
            "task+session diagnostic should be deleted when session is deleted"
        );

        // Project deletion cascades to the remaining diagnostic rows.
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(&project_a)
            .execute(&pool)
            .await
            .expect("delete project");
        let remaining_project_a: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM extension_load_diagnostics WHERE project_id = $1",
        )
        .bind(&project_a)
        .fetch_one(&pool)
        .await
        .expect("count project diagnostics");
        assert_eq!(
            remaining_project_a, 0,
            "project diagnostics should be deleted"
        );

        pool.close().await;
    })
    .await;
}
