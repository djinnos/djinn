//! Migration 95 — `liveness_evidence` + `claim_extensions` tables and
//! additive `sessions` / `task_runs` columns for proposal `twis` /
//! epic `5ric` (Liveness classifier foundation).
//!
//! Verifies the new migration applies cleanly on a fresh database AND on top
//! of the prior schema (so additive ordering is correct), and that the
//! resulting schema carries the columns, CHECK constraints, tables, and
//! indexes the downstream liveness classifier / repository layers rely on.
//!
//! Mirrors the pattern in `migrations_doctor_findings.rs` (migration 61) and
//! `migrations_sessions_parked_reason.rs` (migration 59) so the new test fits
//! the existing harness without inventing a new test infra.
//!
//! Note: this migration is V95 because V94 (`task_attempts`) was merged into
//! main after this task started; both migrations apply cleanly alongside each
//! other.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 95;
const MIGRATION_FILE: &str = "95_liveness_evidence_outcomes.sql";

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
/// `MIGRATION_VERSION`. This is the "prior migrations" path used to verify
/// that migration 95 is additive on top of the entire V1..V94 chain.
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

async fn apply_migration_95(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 95 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 95 after prior migrations");
}

/// Assert every column / constraint / index this migration is supposed to
/// install is present on a fresh database. We rely on the `fresh` and
/// `prior` tests to share the same `assert_schema` body so any drift is
/// caught in both code paths.
async fn assert_liveness_schema(pool: &sqlx::PgPool) {
    // ── sessions: new columns ──────────────────────────────────────────────
    for (column, data_type, nullable) in [
        ("liveness_verdict", "text", "YES"),
        ("liveness_outcome_kind", "text", "YES"),
        ("liveness_outcome_reason", "text", "YES"),
        ("liveness_evidence", "jsonb", "YES"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'sessions' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect sessions.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "sessions.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some(nullable),
            "sessions.{column} nullability should be {nullable}, got {actual_nullable:?}"
        );
    }

    // ── task_runs: new columns ─────────────────────────────────────────────
    for (column, data_type, nullable) in [
        ("liveness_outcome_kind", "text", "YES"),
        ("liveness_outcome_reason", "text", "YES"),
        ("liveness_evidence", "jsonb", "YES"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'task_runs' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect task_runs.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "task_runs.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some(nullable),
            "task_runs.{column} nullability should be {nullable}, got {actual_nullable:?}"
        );
    }

    // ── CHECK constraints on the new columns ───────────────────────────────
    let check_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint \
         WHERE conrelid IN ('sessions'::regclass, 'task_runs'::regclass) \
           AND contype = 'c' \
           AND conname IN ( \
             'sessions_liveness_verdict_check', \
             'sessions_liveness_outcome_kind_check', \
             'sessions_liveness_outcome_reason_check', \
             'task_runs_liveness_outcome_kind_check', \
             'task_runs_liveness_outcome_reason_check' \
           ) \
         ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect liveness CHECK constraints");
    // Postgres returns the constraint names sorted alphabetically (ORDER BY
    // conname above), regardless of the table they live on.
    let mut expected_checks = vec![
        "sessions_liveness_outcome_kind_check",
        "sessions_liveness_outcome_reason_check",
        "sessions_liveness_verdict_check",
        "task_runs_liveness_outcome_kind_check",
        "task_runs_liveness_outcome_reason_check",
    ];
    expected_checks.sort();
    assert_eq!(
        check_constraints, expected_checks,
        "expected liveness CHECK constraints, got {check_constraints:?}"
    );

    // The verdict CHECK constraint should accept exactly the four values
    // listed in the migration header.
    let sessions_verdict_check: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) \
         FROM pg_constraint \
         WHERE conname = 'sessions_liveness_verdict_check'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect sessions_liveness_verdict_check body");
    let body = sessions_verdict_check.unwrap_or_default();
    for value in ["live", "slow", "dead", "protocol_violation"] {
        assert!(
            body.contains(&format!("'{value}'")),
            "sessions_liveness_verdict_check should accept '{value}', got body: {body}"
        );
    }

    // The outcome-kind CHECK constraint should accept every stable outcome
    // kind listed in the epic description.
    let outcome_kind_check: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) \
         FROM pg_constraint \
         WHERE conname = 'sessions_liveness_outcome_kind_check'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect sessions_liveness_outcome_kind_check body");
    let body = outcome_kind_check.unwrap_or_default();
    for value in [
        "success",
        "crash",
        "timeout",
        "dead_reclaimed",
        "protocol_violation",
        "kill_noop",
        "slow_extended",
    ] {
        assert!(
            body.contains(&format!("'{value}'")),
            "sessions_liveness_outcome_kind_check should accept '{value}', got body: {body}"
        );
    }

    // The outcome-reason CHECK constraint should accept every stable reason
    // string listed in the epic description.
    let outcome_reason_check: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) \
         FROM pg_constraint \
         WHERE conname = 'sessions_liveness_outcome_reason_check'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect sessions_liveness_outcome_reason_check body");
    let body = outcome_reason_check.unwrap_or_default();
    for value in [
        "clean_exit_nonterminal",
        "nonzero_exit_nonterminal",
        "hard_runtime_exceeded",
        "slow_extension_budget_exhausted",
    ] {
        assert!(
            body.contains(&format!("'{value}'")),
            "sessions_liveness_outcome_reason_check should accept '{value}', got body: {body}"
        );
    }

    // ── liveness_evidence table ───────────────────────────────────────────
    let liveness_evidence_table: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'liveness_evidence'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect liveness_evidence table");
    assert_eq!(
        liveness_evidence_table.as_deref(),
        Some("liveness_evidence"),
        "liveness_evidence table should exist"
    );

    for (column, data_type, nullable) in [
        ("id", "character varying", "NO"),
        ("session_id", "character varying", "NO"),
        ("task_id", "character varying", "YES"),
        ("task_run_id", "character varying", "YES"),
        ("verdict", "character varying", "NO"),
        ("outcome_kind", "character varying", "YES"),
        ("outcome_reason", "character varying", "YES"),
        ("evidence", "jsonb", "NO"),
        ("created_at", "character varying", "NO"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'liveness_evidence' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect liveness_evidence.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "liveness_evidence.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some(nullable),
            "liveness_evidence.{column} nullability should be {nullable}, got {actual_nullable:?}"
        );
    }

    // Defaults on liveness_evidence: evidence defaults to `{}` and
    // created_at defaults to a to_char() expression.
    let evidence_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'liveness_evidence' AND column_name = 'evidence'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect liveness_evidence.evidence default");
    assert_eq!(
        evidence_default.as_deref(),
        Some("'{}'::jsonb"),
        "liveness_evidence.evidence default should be '{{}}'::jsonb"
    );

    // CHECK constraints on liveness_evidence: verdict / outcome_kind /
    // outcome_reason are validated with the same vocabulary as sessions.
    let liveness_constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint \
         WHERE conrelid = 'liveness_evidence'::regclass \
           AND contype = 'c' \
         ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect liveness_evidence CHECK constraints");
    for expected in [
        "liveness_evidence_verdict_check",
        "liveness_evidence_outcome_kind_check",
        "liveness_evidence_outcome_reason_check",
    ] {
        assert!(
            liveness_constraints.iter().any(|c| c == expected),
            "expected constraint {expected} on liveness_evidence, got {liveness_constraints:?}"
        );
    }

    // Foreign keys: liveness_evidence → sessions/tasks/task_runs.
    let liveness_fks: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint \
         WHERE conrelid = 'liveness_evidence'::regclass \
           AND contype = 'f' \
         ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect liveness_evidence FK constraints");
    for expected in [
        "fk_liveness_evidence_session",
        "fk_liveness_evidence_task",
        "fk_liveness_evidence_task_run",
    ] {
        assert!(
            liveness_fks.iter().any(|c| c == expected),
            "expected FK {expected} on liveness_evidence, got {liveness_fks:?}"
        );
    }

    // ── claim_extensions table ────────────────────────────────────────────
    let claim_extensions_table: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'claim_extensions'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect claim_extensions table");
    assert_eq!(
        claim_extensions_table.as_deref(),
        Some("claim_extensions"),
        "claim_extensions table should exist"
    );

    for (column, data_type, nullable) in [
        ("id", "character varying", "NO"),
        ("session_id", "character varying", "NO"),
        ("task_run_id", "character varying", "YES"),
        ("project_id", "character varying", "NO"),
        ("liveness_evidence_id", "character varying", "YES"),
        ("granted", "boolean", "NO"),
        ("extension_budget_before", "integer", "NO"),
        ("extension_budget_after", "integer", "NO"),
        ("created_at", "character varying", "NO"),
        ("metadata", "jsonb", "NO"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'claim_extensions' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect claim_extensions.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "claim_extensions.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some(nullable),
            "claim_extensions.{column} nullability should be {nullable}, got {actual_nullable:?}"
        );
    }

    // Foreign keys: claim_extensions → liveness_evidence / sessions /
    // task_runs / projects.
    let claim_fks: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint \
         WHERE conrelid = 'claim_extensions'::regclass \
           AND contype = 'f' \
         ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect claim_extensions FK constraints");
    for expected in [
        "claim_extensions_liveness_evidence_fk",
        "fk_claim_extensions_session",
        "fk_claim_extensions_task_run",
        "fk_claim_extensions_project",
    ] {
        assert!(
            claim_fks.iter().any(|c| c == expected),
            "expected FK {expected} on claim_extensions, got {claim_fks:?}"
        );
    }

    // ── indexes ───────────────────────────────────────────────────────────
    let index_names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_index i ON i.indexrelid = c.oid \
         JOIN pg_class t ON t.oid = i.indrelid \
         WHERE t.relname IN ('sessions', 'task_runs', 'liveness_evidence', 'claim_extensions') \
           AND c.relname IN ( \
             'idx_sessions_liveness_verdict', \
             'idx_task_runs_liveness_outcome_kind', \
             'idx_liveness_evidence_session_created', \
             'idx_liveness_evidence_task_run_created', \
             'idx_liveness_evidence_task_created', \
             'idx_liveness_evidence_verdict', \
             'idx_claim_extensions_session_created', \
             'idx_claim_extensions_task_run_created', \
             'idx_claim_extensions_project_created' \
           ) \
         ORDER BY c.relname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect liveness indexes");
    let mut expected_indexes = vec![
        "idx_sessions_liveness_verdict",
        "idx_task_runs_liveness_outcome_kind",
        "idx_liveness_evidence_session_created",
        "idx_liveness_evidence_task_run_created",
        "idx_liveness_evidence_task_created",
        "idx_liveness_evidence_verdict",
        "idx_claim_extensions_session_created",
        "idx_claim_extensions_task_run_created",
        "idx_claim_extensions_project_created",
    ];
    expected_indexes.sort();
    assert_eq!(
        index_names, expected_indexes,
        "expected liveness indexes, got {index_names:?}"
    );

    // The partial indexes use the documented predicates; verify each one
    // has the expected partial predicate (or no predicate at all if it's a
    // full index). We only need to check the ones with predicates.
    let predicate_expectations: &[(&str, &str)] = &[
        (
            "idx_sessions_liveness_verdict",
            "(liveness_verdict IS NOT NULL)",
        ),
        (
            "idx_task_runs_liveness_outcome_kind",
            "(liveness_outcome_kind IS NOT NULL)",
        ),
        (
            "idx_liveness_evidence_task_run_created",
            "(task_run_id IS NOT NULL)",
        ),
        (
            "idx_liveness_evidence_task_created",
            "(task_id IS NOT NULL)",
        ),
        (
            "idx_claim_extensions_task_run_created",
            "(task_run_id IS NOT NULL)",
        ),
    ];
    for (index_name, expected_predicate) in predicate_expectations {
        let actual_predicate: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1",
        )
        .bind(index_name)
        .fetch_optional(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect {index_name} predicate: {e}"));
        assert_eq!(
            actual_predicate.as_deref(),
            Some(*expected_predicate),
            "index {index_name} predicate should be {expected_predicate:?}, got {actual_predicate:?}"
        );
    }
}

/// Seed a project / task / task_run so we can prove that old session and
/// task_run rows from a pre-migration-95 database stay readable after the
/// migration is applied. This is the "old rows remain readable without
/// backfill" invariant from the third acceptance criterion.
async fn seed_legacy_rows(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('project-legacy', 'project-legacy', 'djinnos', 'djinn-legacy')",
    )
    .execute(pool)
    .await
    .expect("seed legacy project");

    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs) \
         VALUES ('task-legacy', 'project-legacy', 'legacy', 'title', 'description', 'design', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
    )
    .execute(pool)
    .await
    .expect("seed legacy task");

    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
         VALUES ('run-legacy', 'project-legacy', 'task-legacy', 'manual', 'pending')",
    )
    .execute(pool)
    .await
    .expect("seed legacy task_run");

    sqlx::query(
        "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status, task_run_id) \
         VALUES ('session-legacy', 'project-legacy', 'task-legacy', 'claude-opus-4-7', 'planner', 'active', 'run-legacy')",
    )
    .execute(pool)
    .await
    .expect("seed legacy session");
}

async fn assert_legacy_rows_readable(pool: &sqlx::PgPool) {
    // The legacy session must still be readable and have NULL liveness
    // columns because the migration is additive.
    let (verdict, outcome_kind, outcome_reason, evidence): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<serde_json::Value>,
    ) = sqlx::query_as(
        "SELECT liveness_verdict, liveness_outcome_kind, liveness_outcome_reason, liveness_evidence \
         FROM sessions WHERE id = 'session-legacy'",
    )
    .fetch_one(pool)
    .await
    .expect("load legacy session");
    assert_eq!(
        verdict, None,
        "legacy session liveness_verdict should be NULL"
    );
    assert_eq!(
        outcome_kind, None,
        "legacy session liveness_outcome_kind should be NULL"
    );
    assert_eq!(
        outcome_reason, None,
        "legacy session liveness_outcome_reason should be NULL"
    );
    assert_eq!(
        evidence, None,
        "legacy session liveness_evidence should be NULL"
    );

    // The legacy task_run must also remain readable.
    let (outcome_kind, outcome_reason, evidence): (
        Option<String>,
        Option<String>,
        Option<serde_json::Value>,
    ) = sqlx::query_as(
        "SELECT liveness_outcome_kind, liveness_outcome_reason, liveness_evidence \
         FROM task_runs WHERE id = 'run-legacy'",
    )
    .fetch_one(pool)
    .await
    .expect("load legacy task_run");
    assert_eq!(
        outcome_kind, None,
        "legacy task_run liveness_outcome_kind should be NULL"
    );
    assert_eq!(
        outcome_reason, None,
        "legacy task_run liveness_outcome_reason should be NULL"
    );
    assert_eq!(
        evidence, None,
        "legacy task_run liveness_evidence should be NULL"
    );
}

#[tokio::test]
async fn migration_95_applies_on_fresh_database() {
    with_temp_database("fresh_liveness", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        sqlx::migrate!("./migrations_postgres")
            .run(&pool)
            .await
            .expect("apply all migrations to fresh database");

        assert_liveness_schema(&pool).await;
        seed_legacy_rows(&pool).await;
        assert_legacy_rows_readable(&pool).await;

        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_95_applies_after_prior_migrations() {
    with_temp_database("prior_liveness", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;

        // Seed legacy rows BEFORE migration 95 applies so we can prove the
        // migration does not require backfill for old session/task_run rows.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect prior migration database (pool)");
        seed_legacy_rows(&pool).await;
        pool.close().await;

        apply_migration_95(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");

        assert_liveness_schema(&pool).await;
        assert_legacy_rows_readable(&pool).await;

        pool.close().await;
    })
    .await;
}
