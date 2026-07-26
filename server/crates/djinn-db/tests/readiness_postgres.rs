//! Real-Postgres contract tests for migration 155 readiness persistence.
//!
//! These tests deliberately create a fresh database, replay the migrations
//! before 155, and execute migration 155 directly.  The constraints below are
//! therefore exercised by Postgres itself rather than an in-memory substitute.

use std::path::{Path, PathBuf};

use djinn_db::{
    Database,
    repositories::readiness::{CreateReadinessRun, ReadinessRepository},
};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 155;
const MIGRATION_FILE: &str = "155_readiness_persistence.sql";
const DESIGNATED_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000155";

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read migrations")
        .filter_map(|entry| {
            let path = entry.expect("migration entry").path();
            let name = path.file_name()?.to_str()?;
            let version = name.split_once('_')?.0.parse().ok()?;
            (path.extension()?.to_str()? == "sql").then_some((version, path))
        })
        .collect();
    entries.sort_by_key(|(version, _)| *version);
    entries
}

async fn apply_prior_migrations(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        // Migration 142 deliberately requires a validated operator GUC even
        // when this fresh fixture has no tasks to backfill.
        if version == 142 {
            sqlx::query(
                "INSERT INTO users (id,github_id,github_login,is_member_of_org) \
                 VALUES ($1,155,'readiness-migration-operator',true)",
            )
            .bind(DESIGNATED_OPERATOR_ID)
            .execute(&mut *conn)
            .await
            .expect("seed migration 142 designated operator");
            sqlx::query(
                "SELECT set_config('djinn.migration_designated_operator_user_id',$1,false)",
            )
            .bind(DESIGNATED_OPERATOR_ID)
            .execute(&mut *conn)
            .await
            .expect("set migration 142 designated operator");
        }
        conn.execute(
            std::fs::read_to_string(&path)
                .expect("read migration")
                .as_str(),
        )
        .await
        .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    }
}

async fn apply_readiness_migration(conn: &mut PgConnection) {
    let path = migrations_dir().join(MIGRATION_FILE);
    conn.execute(
        std::fs::read_to_string(&path)
            .expect("read readiness migration")
            .as_str(),
    )
    .await
    .expect("apply readiness migration");
}

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = djinn_db::test_database_base_url();
    let prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base);
    let name = format!("djinn_readiness_{suffix}_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin connect");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create test database");
    drop(admin);

    let database_url = format!("{prefix}/{name}");
    let result = f(database_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin reconnect");
    let _ = admin.execute(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}' AND pid <> pg_backend_pid()").as_str()).await;
    let _ = admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{name}""#).as_str())
        .await;
    result
}

async fn migrated_connection(url: &str) -> PgConnection {
    let mut conn = PgConnection::connect(url)
        .await
        .expect("connect fresh database");
    apply_prior_migrations(&mut conn).await;
    apply_readiness_migration(&mut conn).await;
    conn
}

async fn seed_project(conn: &mut PgConnection, id: &str) {
    sqlx::query(
        "INSERT INTO projects (id,name,github_owner,github_repo) VALUES ($1,$2,'djinnos',$3)",
    )
    .bind(id)
    .bind(format!("project-{id}"))
    .bind(format!("repo-{id}"))
    .execute(&mut *conn)
    .await
    .expect("seed project");
}

async fn run(conn: &mut PgConnection, id: &str, project: &str, key: &str) {
    sqlx::query("INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ($1,$2,$3,'snapshot','skill','1.0.0')")
        .bind(id).bind(project).bind(key).execute(&mut *conn).await.expect("insert run");
}

async fn area(conn: &mut PgConnection, id: &str, run_id: &str, key: &str) {
    sqlx::query("INSERT INTO readiness_composition_areas (id,run_id,area_key,composition,path_scopes) VALUES ($1,$2,$3,'{}','[]')")
        .bind(id).bind(run_id).bind(key).execute(&mut *conn).await.expect("insert area");
}

async fn attempt(
    conn: &mut PgConnection,
    id: &str,
    run_id: &str,
    area_id: &str,
    number: i32,
    correlation: &str,
) {
    sqlx::query("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,$4,$5)")
        .bind(id).bind(run_id).bind(area_id).bind(number).bind(correlation)
        .execute(&mut *conn).await.expect("insert attempt");
}

async fn assert_rejected(conn: &mut PgConnection, sql: &str, marker: &str) {
    let error = conn.execute(sql).await.expect_err("write must be rejected");
    assert!(
        error.to_string().contains(marker),
        "expected {marker}, got {error}"
    );
}

#[tokio::test]
async fn migration_constraints_reject_identity_correlation_and_lifecycle_violations() {
    with_temp_database("constraints", |url| async move {
        let mut conn = migrated_connection(&url).await;
        seed_project(&mut conn, "project-a").await;
        seed_project(&mut conn, "project-b").await;
        run(&mut conn, "run-a", "project-a", "key-a").await;

        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ('run-duplicate','project-a','key-a','s','s','1')", "readiness_runs_project_idempotency_key").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ('run-active','project-a','key-b','s','s','1')", "readiness_runs_one_active_project_idx").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version) VALUES ('bad-status','project-b','bad','unknown','s','s','1')", "readiness_runs_status_check").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version) VALUES ('bad-terminal','project-b','terminal','completed','s','s','1')", "readiness_runs_terminal_check").await;

        area(&mut conn, "area-a", "run-a", "frontend").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_composition_areas (id,run_id,area_key) VALUES ('area-duplicate','run-a','frontend')", "readiness_areas_run_key").await;
        attempt(&mut conn, "attempt-a", "run-a", "area-a", 1, "correlation-a").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('attempt-number','run-a','area-a',1,'correlation-b')", "readiness_attempts_area_number").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('attempt-correlation','run-a','area-a',2,'correlation-a')", "readiness_attempts_correlation_key").await;
        assert_rejected(&mut conn, "UPDATE readiness_area_attempts SET status='succeeded' WHERE id='attempt-a'", "readiness_attempts_terminal_check").await;

        run(&mut conn, "run-b", "project-b", "key-b").await;
        area(&mut conn, "area-b", "run-b", "backend").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('cross-attempt','run-b','area-a',3,'cross-run-correlation')", "readiness_attempts_area_run_fk").await;
        sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity,accepted) VALUES ('finding-a','run-a','area-a','attempt-a','guardrail','high',true)").execute(&mut conn).await.expect("insert finding");
        assert_rejected(&mut conn, "INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('finding-duplicate','run-a','area-a','attempt-a','guardrail','high')", "readiness_findings_attempt_guardrail").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('finding-cross','run-b','area-b','attempt-a','other','high')", "readiness_findings_attempt_correlation_fk").await;
        sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('suggestion-a','run-a','dedupe','{}')").execute(&mut conn).await.expect("insert suggestion");
        assert_rejected(&mut conn, "INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('suggestion-duplicate','run-a','dedupe','{}')", "readiness_suggestions_run_dedupe").await;
    }).await;
}

#[tokio::test]
async fn repository_concurrent_start_has_one_active_run_and_same_key_resolves() {
    // Database::ephemeral is a template-cloned, real Postgres database.
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    djinn_db::test_support::seed_project(&db, "readiness-concurrency", "readiness").await;
    let repo = ReadinessRepository::new(db.clone());
    let input = |key: &str| CreateReadinessRun {
        project_id: "readiness-concurrency".into(),
        idempotency_key: key.into(),
        repository_snapshot: "snapshot".into(),
        skill_name: "skill".into(),
        skill_version: "1.0.0".into(),
    };
    let (left, right) = tokio::join!(
        repo.create_run(input("left")),
        repo.create_run(input("right"))
    );
    assert_eq!(
        usize::from(left.is_ok()) + usize::from(right.is_ok()),
        1,
        "only one active run may be created"
    );
    let winner = left.as_ref().ok().or(right.as_ref().ok()).expect("winner");
    let duplicate = repo
        .create_run(input(&winner.idempotency_key))
        .await
        .expect("same idempotency key resolves");
    assert_eq!(duplicate.id, winner.id);
    let active = repo
        .active_or_latest_for_project("readiness-concurrency")
        .await
        .expect("load active run")
        .expect("active run exists");
    assert_eq!(
        active.id, winner.id,
        "active run is preferred by repository query"
    );

    // Once the sole active row becomes terminal, the same repository method
    // must fall back to the most recently-created terminal run rather than an
    // arbitrary (or oldest) project run.
    sqlx::query(
        "UPDATE readiness_runs SET status='failed', completed_at='2026-01-01T00:00:00.000Z', \
         created_at='2026-01-01T00:00:00.000Z' WHERE id=$1",
    )
    .bind(&winner.id)
    .execute(db.pool())
    .await
    .expect("terminalize active run");
    sqlx::query(
        "INSERT INTO readiness_runs \
         (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,created_at,completed_at) \
         VALUES ('older-terminal','readiness-concurrency','older','failed','snapshot','skill','1.0.0', \
                 '2025-01-01T00:00:00.000Z','2025-01-01T00:00:00.000Z')",
    )
    .execute(db.pool())
    .await
    .expect("create older terminal run");
    let latest_terminal = repo
        .active_or_latest_for_project("readiness-concurrency")
        .await
        .expect("load latest terminal run")
        .expect("terminal run exists");
    assert_eq!(
        latest_terminal.id, winner.id,
        "repository falls back to the newest terminal run when no run is active"
    );
}

#[tokio::test]
async fn active_latest_and_detail_indexes_have_expected_query_paths() {
    with_temp_database("indexes", |url| async move {
        let mut conn = migrated_connection(&url).await;
        seed_project(&mut conn, "project-index").await;
        run(&mut conn, "run-old", "project-index", "old").await;
        sqlx::query("UPDATE readiness_runs SET status='failed', completed_at='2025-01-01T00:00:00.000Z', created_at='2025-01-01T00:00:00.000Z' WHERE id='run-old'").execute(&mut conn).await.expect("terminalize old");
        run(&mut conn, "run-active", "project-index", "active").await;
        area(&mut conn, "area-index", "run-active", "ui").await;
        attempt(&mut conn, "attempt-index", "run-active", "area-index", 1, "index-correlation").await;
        let selected: String = sqlx::query_scalar("SELECT id FROM readiness_runs WHERE project_id=$1 ORDER BY (status IN ('identifying','analyzing','aggregating')) DESC,created_at DESC LIMIT 1").bind("project-index").fetch_one(&mut conn).await.expect("active/latest query");
        assert_eq!(selected, "run-active", "active run wins over a terminal latest candidate");
        conn.execute("SET enable_seqscan = off").await.expect("force index plans");
        for (sql, index) in [
            ("EXPLAIN SELECT * FROM readiness_runs WHERE project_id='project-index' AND status IN ('identifying','analyzing','aggregating')", "readiness_runs_one_active_project_idx"),
            ("EXPLAIN SELECT * FROM readiness_runs WHERE project_id='project-index' ORDER BY created_at DESC", "readiness_runs_project_latest_idx"),
            ("EXPLAIN SELECT * FROM readiness_composition_areas WHERE run_id='run-active' ORDER BY area_key", "readiness_areas_run_detail_idx"),
            ("EXPLAIN SELECT * FROM readiness_area_attempts WHERE area_id='area-index' ORDER BY attempt_number DESC", "readiness_attempts_area_idx"),
            ("EXPLAIN SELECT * FROM readiness_run_events WHERE run_id='run-active' ORDER BY created_at,id", "readiness_events_run_detail_idx"),
        ] {
            let plan: Vec<String> = sqlx::query_scalar(sql).fetch_all(&mut conn).await.expect("explain query path");
            assert!(plan.join("\n").contains(index), "expected {index} in plan: {plan:?}");
        }
    }).await;
}

#[tokio::test]
async fn frozen_and_completed_readiness_data_is_immutable() {
    with_temp_database("immutable", |url| async move {
        let mut conn = migrated_connection(&url).await;
        seed_project(&mut conn, "project-immutable").await;
        run(&mut conn, "run-immutable", "project-immutable", "immutable").await;
        area(&mut conn, "area-immutable", "run-immutable", "frozen").await;
        sqlx::query("UPDATE readiness_composition_areas SET status='running' WHERE id='area-immutable'").execute(&mut conn).await.expect("status transition allowed");
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET composition='{\"changed\":true}' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET path_scopes='[\"changed\"]' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET area_key='changed' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET run_id='other-run' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        attempt(&mut conn, "attempt-immutable", "run-immutable", "area-immutable", 1, "immutable-correlation").await;
        sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity,accepted) VALUES ('finding-immutable','run-immutable','area-immutable','attempt-immutable','guardrail','high',true)").execute(&mut conn).await.expect("accepted finding");
        assert_rejected(&mut conn, "UPDATE readiness_guardrail_findings SET severity='low' WHERE id='finding-immutable'", "accepted readiness finding is immutable").await;
        assert_rejected(&mut conn, "DELETE FROM readiness_guardrail_findings WHERE id='finding-immutable'", "accepted readiness finding is immutable").await;
        sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('finding-unaccepted','run-immutable','area-immutable','attempt-immutable','unaccepted','low')").execute(&mut conn).await.expect("unaccepted finding");
        sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('suggestion-immutable','run-immutable','dedupe','{}')").execute(&mut conn).await.expect("suggestion");
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ('event-immutable','run-immutable','created','{}')").execute(&mut conn).await.expect("event");
        assert_rejected(&mut conn, "UPDATE readiness_run_events SET event_kind='changed' WHERE id='event-immutable'", "readiness run events are append-only").await;
        sqlx::query("UPDATE readiness_area_attempts SET status='succeeded', terminal_at='2025-01-01T00:00:00.000Z' WHERE id='attempt-immutable'").execute(&mut conn).await.expect("terminal attempt");
        sqlx::query("UPDATE readiness_composition_areas SET status='succeeded' WHERE id='area-immutable'").execute(&mut conn).await.expect("terminal area");
        sqlx::query("UPDATE readiness_runs SET expected_area_count=1,status='completed',completed_at='2025-01-01T00:00:00.000Z' WHERE id='run-immutable'").execute(&mut conn).await.expect("complete run");
        for sql in [
            "INSERT INTO readiness_composition_areas (id,run_id,area_key) VALUES ('after-area','run-immutable','after')",
            "UPDATE readiness_composition_areas SET status='failed' WHERE id='area-immutable'",
            "DELETE FROM readiness_composition_areas WHERE id='area-immutable'",
            "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('after-attempt','run-immutable','area-immutable',2,'after')",
            "UPDATE readiness_area_attempts SET payload_digest='changed' WHERE id='attempt-immutable'",
            "DELETE FROM readiness_area_attempts WHERE id='attempt-immutable'",
            "INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('after-finding','run-immutable','area-immutable','attempt-immutable','after','low')",
            "UPDATE readiness_guardrail_findings SET severity='high' WHERE id='finding-unaccepted'",
            "INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('after-suggestion','run-immutable','after','{}')",
            "DELETE FROM readiness_remediation_suggestions WHERE id='suggestion-immutable'",
            "INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ('after-event','run-immutable','after','{}')",
        ] { assert_rejected(&mut conn, sql, "readiness run is terminal").await; }

        // The append-only trigger is deliberately stronger than the terminal
        // child-write trigger for deletes, so assert its stable contract here.
        assert_rejected(&mut conn, "DELETE FROM readiness_run_events WHERE id='event-immutable'", "readiness run events are append-only").await;

        // Isolate the completed-run finding delete guard from the independent
        // guard which rejects every finding delete (accepted or otherwise).
        // The unaccepted update above exercises the normal trigger stack.
        conn.execute("ALTER TABLE readiness_guardrail_findings DISABLE TRIGGER readiness_findings_immutable_delete").await.expect("disable independent finding delete guard");
        assert_rejected(&mut conn, "DELETE FROM readiness_guardrail_findings WHERE id='finding-unaccepted'", "readiness run is terminal").await;
        conn.execute("ALTER TABLE readiness_guardrail_findings ENABLE TRIGGER readiness_findings_immutable_delete").await.expect("restore independent finding delete guard");
        assert_rejected(&mut conn, "UPDATE readiness_runs SET status='failed' WHERE id='run-immutable'", "completed readiness run is immutable").await;
    }).await;
}
