//! PostgreSQL contract tests for the extension-load diagnostic durable store.

use std::sync::Arc;

use djinn_core::extension_diagnostics::{
    ExtensionLoadPhase, ExtensionLoadRemedyCode, ExtensionLoadSeverity, ExtensionLoadSourceKind,
};
use futures::future::join_all;
use sqlx::Row;
use tokio::sync::Barrier;

use crate::Database;
use crate::repositories::extension_load_diagnostic::{
    ExtensionLoadDiagnosticRepository, InsertExtensionLoadDiagnostic,
};
use crate::repositories::test_support::{
    UsageTestSessionSeed, UsageTestTaskSeed, seed_project, seed_session_row_with_id, seed_task_row,
};

const TIMESTAMP: &str = "2026-07-13T10:00:00.000Z";

struct Fixture {
    db: Database,
    project_id: String,
    session_id: String,
    task_id: String,
}

async fn fixture(name: &str) -> Fixture {
    let db = Database::open_in_memory().expect("database");
    let project_id = uuid::Uuid::now_v7().to_string();
    seed_project(&db, &project_id, name).await;
    let task_id = seed_task_row(
        &db,
        UsageTestTaskSeed {
            project_id: &project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_session_row_with_id(
        &db,
        &session_id,
        UsageTestSessionSeed {
            project_id: &project_id,
            model_id: "test-model",
            agent_type: "worker",
            started_at: TIMESTAMP,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: None,
            cost_basis: "unpriced",
            task_id: Some(&task_id),
        },
    )
    .await;
    Fixture {
        db,
        project_id,
        session_id,
        task_id,
    }
}

fn input(fixture: &Fixture, attempt: &str) -> InsertExtensionLoadDiagnostic {
    InsertExtensionLoadDiagnostic {
        project_id: fixture.project_id.clone(),
        task_id: Some(fixture.task_id.clone()),
        session_id: Some(fixture.session_id.clone()),
        load_attempt_id: attempt.to_owned(),
        source_kind: ExtensionLoadSourceKind::ProjectMcp,
        source_key: "search".to_owned(),
        phase: ExtensionLoadPhase::ToolsList,
        severity: ExtensionLoadSeverity::Error,
        summary: "tools/list returned invalid JSON".to_owned(),
        summary_fingerprint: "a".repeat(64),
        remedy_code: ExtensionLoadRemedyCode::CheckServer,
        remedy: "Check the MCP server health.".to_owned(),
        first_seen_at: TIMESTAMP.to_owned(),
        last_seen_at: TIMESTAMP.to_owned(),
        created_at: TIMESTAMP.to_owned(),
    }
}

fn doctor_input(fixture: &Fixture, attempt: &str) -> InsertExtensionLoadDiagnostic {
    let mut input = input(fixture, attempt);
    input.task_id = None;
    input.session_id = None;
    input
}

async fn raw_insert(
    db: &Database,
    fixture: &Fixture,
    schema_version: i16,
    task_id: Option<&str>,
    source_kind: &str,
    phase: &str,
    severity: &str,
    remedy_code: &str,
    occurrence_count: i32,
) -> sqlx::Result<sqlx::postgres::PgQueryResult> {
    sqlx::query(
        "INSERT INTO extension_load_diagnostics \
         (id, project_id, task_id, session_id, load_attempt_id, schema_version, source_kind, source_key, phase, severity, summary, summary_fingerprint, remedy_code, remedy, occurrence_count, first_seen_at, last_seen_at, created_at) \
         VALUES ($1, $2, $3, NULL, $4, $5, $6, 'source', $7, $8, 'summary', $9, $10, 'remedy', $11, $12, $12, $12)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&fixture.project_id)
    .bind(task_id)
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(schema_version)
    .bind(source_kind)
    .bind(phase)
    .bind(severity)
    .bind(uuid::Uuid::now_v7().simple().to_string())
    .bind(remedy_code)
    .bind(occurrence_count)
    .bind(TIMESTAMP)
    .execute(db.pool())
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_retry_and_ordering_contract() {
    let fixture = fixture("extension-diagnostic-identity").await;
    let repo = ExtensionLoadDiagnosticRepository::new(fixture.db.clone());

    let first = repo
        .insert_or_increment(input(&fixture, "attempt-one"))
        .await
        .unwrap();
    let retry = repo
        .insert_or_increment(input(&fixture, "attempt-one"))
        .await
        .unwrap();
    let later = repo
        .insert_or_increment(input(&fixture, "attempt-two"))
        .await
        .unwrap();
    let mut skill = input(&fixture, "attempt-one");
    skill.source_kind = ExtensionLoadSourceKind::ProjectSkill;
    skill.source_key = "alpha-skill".to_owned();
    skill.summary_fingerprint = "b".repeat(64);
    let distinct_source = repo.insert_or_increment(skill).await.unwrap();

    assert_eq!(first.diagnostic_id, retry.diagnostic_id);
    assert_eq!(retry.occurrence_count, 2);
    assert_ne!(first.diagnostic_id, later.diagnostic_id);
    assert_ne!(first.diagnostic_id, distinct_source.diagnostic_id);

    let mut mcp_alpha = input(&fixture, "ordered");
    mcp_alpha.source_key = "alpha".to_owned();
    mcp_alpha.summary_fingerprint = "c".repeat(64);
    let mut mcp_alpha_handshake = input(&fixture, "ordered");
    mcp_alpha_handshake.source_key = "alpha".to_owned();
    mcp_alpha_handshake.phase = ExtensionLoadPhase::Handshake;
    mcp_alpha_handshake.summary_fingerprint = "d".repeat(64);
    let mut mcp_beta = input(&fixture, "ordered");
    mcp_beta.source_key = "beta".to_owned();
    mcp_beta.phase = ExtensionLoadPhase::Handshake;
    mcp_beta.summary_fingerprint = "e".repeat(64);
    let mut skill_alpha = input(&fixture, "ordered");
    skill_alpha.source_kind = ExtensionLoadSourceKind::ProjectSkill;
    skill_alpha.source_key = "alpha".to_owned();
    skill_alpha.summary_fingerprint = "f".repeat(64);
    let mut warning = input(&fixture, "ordered");
    warning.severity = ExtensionLoadSeverity::Warning;
    warning.source_key = "aaa".to_owned();
    warning.summary_fingerprint = "g".repeat(64);
    for diagnostic in [
        mcp_alpha,
        mcp_alpha_handshake,
        mcp_beta,
        skill_alpha,
        warning,
    ] {
        repo.insert_or_increment(diagnostic).await.unwrap();
    }

    let ordered = repo
        .list_for_load_attempt(&fixture.project_id, "ordered")
        .await
        .unwrap();
    assert_eq!(ordered.len(), 5);
    assert_eq!(ordered[0].severity, ExtensionLoadSeverity::Error);
    assert_eq!(ordered[0].source_kind, ExtensionLoadSourceKind::ProjectMcp);
    assert_eq!(ordered[0].source_key, "alpha");
    assert_eq!(ordered[0].phase, ExtensionLoadPhase::Handshake);
    assert_eq!(ordered[1].source_key, "alpha");
    assert_eq!(ordered[1].phase, ExtensionLoadPhase::ToolsList);
    assert_eq!(ordered[2].source_key, "beta");
    assert_eq!(
        ordered[3].source_kind,
        ExtensionLoadSourceKind::ProjectSkill
    );
    assert_eq!(ordered[4].severity, ExtensionLoadSeverity::Warning);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_reads_are_project_owned_and_empty_when_absent() {
    let one = fixture("extension-diagnostic-scope-one").await;
    let two = fixture("extension-diagnostic-scope-two").await;
    let one_repo = ExtensionLoadDiagnosticRepository::new(one.db.clone());
    // Each in-memory database is isolated, so seed both project fixtures into the
    // first database for actual cross-project access checks.
    seed_project(
        &one.db,
        &two.project_id,
        "extension-diagnostic-scope-two-copy",
    )
    .await;
    let two_task_id = seed_task_row(
        &one.db,
        UsageTestTaskSeed {
            project_id: &two.project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    seed_session_row_with_id(
        &one.db,
        &two.session_id,
        UsageTestSessionSeed {
            project_id: &two.project_id,
            model_id: "test-model",
            agent_type: "worker",
            started_at: TIMESTAMP,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: None,
            cost_basis: "unpriced",
            task_id: Some(&two_task_id),
        },
    )
    .await;
    let two_in_one = Fixture {
        db: one.db.clone(),
        project_id: two.project_id,
        session_id: two.session_id,
        task_id: two_task_id,
    };
    one_repo
        .insert_or_increment(input(&one, "shared-attempt"))
        .await
        .unwrap();
    one_repo
        .insert_or_increment(input(&two_in_one, "shared-attempt"))
        .await
        .unwrap();

    assert_eq!(
        one_repo
            .list_for_session(&one.project_id, &one.session_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        one_repo
            .list_for_task(&one.project_id, &one.task_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        one_repo
            .list_for_load_attempt(&one.project_id, "shared-attempt")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        one_repo
            .list_for_session(&two_in_one.project_id, &one.session_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        one_repo
            .list_for_task(&two_in_one.project_id, &one.task_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        one_repo
            .list_for_load_attempt(&two_in_one.project_id, "missing-attempt")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        one_repo
            .list_for_session(&one.project_id, "missing-session")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        one_repo
            .list_for_task(&one.project_id, "missing-task")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_attempt_dedupe_is_atomic() {
    let fixture = fixture("extension-diagnostic-concurrent").await;
    let writers = 12_u32;
    let barrier = Arc::new(Barrier::new(writers as usize));
    let writes = (0..writers).map(|_| {
        let repo = ExtensionLoadDiagnosticRepository::new(fixture.db.clone());
        let barrier = barrier.clone();
        let diagnostic = input(&fixture, "concurrent-attempt");
        async move {
            barrier.wait().await;
            repo.insert_or_increment(diagnostic).await.unwrap()
        }
    });
    let persisted = join_all(writes).await;
    assert!(
        persisted
            .iter()
            .all(|row| row.diagnostic_id == persisted[0].diagnostic_id)
    );

    let repo = ExtensionLoadDiagnosticRepository::new(fixture.db.clone());
    let rows = repo
        .list_for_load_attempt(&fixture.project_id, "concurrent-attempt")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].occurrence_count, u64::from(writers));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_schema_guards_reject_invalid_rows_and_allow_doctor_rows() {
    let fixture = fixture("extension-diagnostic-schema").await;
    let db = &fixture.db;
    let constraints: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conrelid = 'extension_load_diagnostics'::regclass \
         AND conname = ANY($1)",
    )
    .bind(vec![
        "chk_extension_load_diagnostics_schema_version",
        "chk_extension_load_diagnostics_association",
        "chk_extension_load_diagnostics_severity",
        "chk_extension_load_diagnostics_source_kind",
        "chk_extension_load_diagnostics_phase",
        "chk_extension_load_diagnostics_remedy_code",
        "chk_extension_load_diagnostics_occurrence_count",
        "fk_extension_load_diagnostics_project",
        "fk_extension_load_diagnostics_task",
        "fk_extension_load_diagnostics_session",
    ])
    .fetch_one(db.pool()).await.unwrap();
    assert_eq!(constraints, 10);
    let indexes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE schemaname = current_schema() \
         AND tablename = 'extension_load_diagnostics' AND indexname = ANY($1)",
    )
    .bind(vec![
        "idx_extension_load_diagnostics_project_id",
        "idx_extension_load_diagnostics_session_id",
        "idx_extension_load_diagnostics_task_id",
        "idx_extension_load_diagnostics_load_attempt_id",
        "idx_extension_load_diagnostics_order",
        "uq_extension_load_diagnostics_dedupe",
    ])
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(indexes, 6);

    let repo = ExtensionLoadDiagnosticRepository::new(db.clone());
    let doctor = repo
        .insert_or_increment(doctor_input(&fixture, "doctor-attempt"))
        .await
        .unwrap();
    assert!(doctor.task_id.is_none() && doctor.session_id.is_none());

    for (name, schema_version, task_id, source_kind, phase, severity, remedy_code, count) in [
        (
            "schema version",
            2,
            None,
            "project_mcp",
            "tools_list",
            "error",
            "check_server",
            1,
        ),
        (
            "severity",
            1,
            None,
            "project_mcp",
            "tools_list",
            "fatal",
            "check_server",
            1,
        ),
        (
            "source kind",
            1,
            None,
            "other",
            "tools_list",
            "error",
            "check_server",
            1,
        ),
        (
            "phase",
            1,
            None,
            "project_mcp",
            "other",
            "error",
            "check_server",
            1,
        ),
        (
            "remedy code",
            1,
            None,
            "project_mcp",
            "tools_list",
            "error",
            "other",
            1,
        ),
        (
            "occurrence count",
            1,
            None,
            "project_mcp",
            "tools_list",
            "error",
            "check_server",
            0,
        ),
        (
            "task without session",
            1,
            Some(fixture.task_id.as_str()),
            "project_mcp",
            "tools_list",
            "error",
            "check_server",
            1,
        ),
    ] {
        assert!(
            raw_insert(
                db,
                &fixture,
                schema_version,
                task_id,
                source_kind,
                phase,
                severity,
                remedy_code,
                count
            )
            .await
            .is_err(),
            "{name} must be rejected"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_and_project_deletion_follow_diagnostic_retention_rules() {
    let fixture = fixture("extension-diagnostic-lifecycle").await;
    let repo = ExtensionLoadDiagnosticRepository::new(fixture.db.clone());
    repo.insert_or_increment(input(&fixture, "session-attempt"))
        .await
        .unwrap();
    let doctor = repo
        .insert_or_increment(doctor_input(&fixture, "doctor-attempt"))
        .await
        .unwrap();

    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(&fixture.session_id)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let after_session: i64 = sqlx::query(
        "SELECT count(*) AS count FROM extension_load_diagnostics WHERE project_id = $1",
    )
    .bind(&fixture.project_id)
    .fetch_one(fixture.db.pool())
    .await
    .unwrap()
    .try_get("count")
    .unwrap();
    assert_eq!(after_session, 1);
    assert_eq!(
        repo.list_for_load_attempt(&fixture.project_id, "doctor-attempt")
            .await
            .unwrap()[0]
            .diagnostic_id,
        doctor.diagnostic_id
    );

    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(&fixture.project_id)
        .execute(fixture.db.pool())
        .await
        .unwrap();
    let after_project: i64 =
        sqlx::query("SELECT count(*) AS count FROM extension_load_diagnostics WHERE id = $1")
            .bind(&doctor.diagnostic_id)
            .fetch_one(fixture.db.pool())
            .await
            .unwrap()
            .try_get("count")
            .unwrap();
    assert_eq!(after_project, 0);
}
