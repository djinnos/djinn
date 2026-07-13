//! Durable, atomic persistence for V1 extension-load diagnostics.
//!
//! The unique index installed by migration 110 defines a diagnostic identity
//! within one load attempt. Writes use that index directly through one
//! `INSERT ... ON CONFLICT ... DO UPDATE` statement, so retries cannot lose an
//! occurrence increment through a read-then-write race.

use djinn_core::extension_diagnostics::{
    EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION, ExtensionLoadDiagnosticV1, ExtensionLoadPhase,
    ExtensionLoadRemedyCode, ExtensionLoadSeverity, ExtensionLoadSourceKind,
};
use sqlx::Row;

use crate::database::Database;
use crate::{Error, Result};

/// Already-normalized input for one extension-load diagnostic observation.
///
/// `summary`, `remedy`, and `summary_fingerprint` are prepared by the trusted
/// caller before they reach the repository. The repository generates the
/// diagnostic id only when the input is first persisted for its dedupe key.
#[derive(Clone, Debug)]
pub struct InsertExtensionLoadDiagnostic {
    pub project_id: String,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub load_attempt_id: String,
    pub source_kind: ExtensionLoadSourceKind,
    pub source_key: String,
    pub phase: ExtensionLoadPhase,
    pub severity: ExtensionLoadSeverity,
    pub summary: String,
    pub summary_fingerprint: String,
    pub remedy_code: ExtensionLoadRemedyCode,
    pub remedy: String,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub created_at: String,
}

/// Project-owned repository for extension-load diagnostic records.
pub struct ExtensionLoadDiagnosticRepository {
    db: Database,
}

impl ExtensionLoadDiagnosticRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Persist an observation, atomically incrementing an equivalent row in
    /// the same load attempt. The generated id is bound once; PostgreSQL keeps
    /// the existing id on conflict and returns that persisted row.
    pub async fn insert_or_increment(
        &self,
        input: InsertExtensionLoadDiagnostic,
    ) -> Result<ExtensionLoadDiagnosticV1> {
        self.db.ensure_initialized().await?;
        let diagnostic_id = uuid::Uuid::now_v7().to_string();
        let row = sqlx::query(
            r#"INSERT INTO extension_load_diagnostics
                (id, project_id, task_id, session_id, load_attempt_id, schema_version,
                 source_kind, source_key, phase, severity, summary, summary_fingerprint,
                 remedy_code, remedy, occurrence_count, first_seen_at, last_seen_at, created_at)
               VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, 1, $15, $16, $17)
               ON CONFLICT
                (project_id, task_id, session_id, load_attempt_id, source_kind, source_key,
                 phase, remedy_code, summary_fingerprint)
               DO UPDATE SET
                 occurrence_count = extension_load_diagnostics.occurrence_count + 1,
                 last_seen_at = EXCLUDED.last_seen_at
               RETURNING id, project_id, task_id, session_id, load_attempt_id, schema_version,
                 source_kind, source_key, phase, severity, summary, remedy_code, remedy,
                 occurrence_count, first_seen_at, last_seen_at, created_at"#,
        )
        .bind(diagnostic_id)
        .bind(&input.project_id)
        .bind(input.task_id.as_deref())
        .bind(input.session_id.as_deref())
        .bind(&input.load_attempt_id)
        .bind(EXTENSION_LOAD_DIAGNOSTIC_V1_SCHEMA_VERSION)
        .bind(input.source_kind.as_str())
        .bind(&input.source_key)
        .bind(input.phase.as_str())
        .bind(input.severity.as_str())
        .bind(&input.summary)
        .bind(&input.summary_fingerprint)
        .bind(input.remedy_code.as_str())
        .bind(&input.remedy)
        .bind(&input.first_seen_at)
        .bind(&input.last_seen_at)
        .bind(&input.created_at)
        .fetch_one(self.db.pool())
        .await?;
        row_to_diagnostic(&row)
    }

    /// List diagnostics associated with a session in one project.
    pub async fn list_for_session(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> Result<Vec<ExtensionLoadDiagnosticV1>> {
        self.list_scoped("session_id = $2", project_id, session_id)
            .await
    }

    /// List diagnostics associated with a task in one project.
    pub async fn list_for_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<Vec<ExtensionLoadDiagnosticV1>> {
        self.list_scoped("task_id = $2", project_id, task_id).await
    }

    /// List diagnostics in one load attempt in one project.
    pub async fn list_for_load_attempt(
        &self,
        project_id: &str,
        load_attempt_id: &str,
    ) -> Result<Vec<ExtensionLoadDiagnosticV1>> {
        self.list_scoped("load_attempt_id = $2", project_id, load_attempt_id)
            .await
    }

    async fn list_scoped(
        &self,
        association_predicate: &str,
        project_id: &str,
        association_id: &str,
    ) -> Result<Vec<ExtensionLoadDiagnosticV1>> {
        self.db.ensure_initialized().await?;
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM extension_load_diagnostics \
             WHERE project_id = $1 AND {association_predicate} \
             ORDER BY severity ASC, source_kind ASC, source_key ASC, phase ASC, id ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(project_id)
            .bind(association_id)
            .fetch_all(self.db.pool())
            .await?;
        rows.iter().map(row_to_diagnostic).collect()
    }
}

const SELECT_COLUMNS: &str = "id, project_id, task_id, session_id, load_attempt_id, schema_version, \
    source_kind, source_key, phase, severity, summary, remedy_code, remedy, occurrence_count, \
    first_seen_at, last_seen_at, created_at";

fn row_to_diagnostic(row: &sqlx::postgres::PgRow) -> Result<ExtensionLoadDiagnosticV1> {
    let schema_version: i16 = row.try_get("schema_version")?;
    let occurrence_count: i32 = row.try_get("occurrence_count")?;
    Ok(ExtensionLoadDiagnosticV1 {
        schema_version: i32::from(schema_version),
        diagnostic_id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        task_id: row.try_get("task_id")?,
        session_id: row.try_get("session_id")?,
        load_attempt_id: row.try_get("load_attempt_id")?,
        source_kind: parse_source_kind(&row.try_get::<String, _>("source_kind")?)?,
        source_key: row.try_get("source_key")?,
        phase: parse_phase(&row.try_get::<String, _>("phase")?)?,
        severity: parse_severity(&row.try_get::<String, _>("severity")?)?,
        summary: row.try_get("summary")?,
        remedy_code: parse_remedy_code(&row.try_get::<String, _>("remedy_code")?)?,
        remedy: row.try_get("remedy")?,
        occurrence_count: occurrence_count
            .try_into()
            .map_err(|_| Error::InvalidData("negative diagnostic occurrence count".to_owned()))?,
        first_seen_at: row.try_get("first_seen_at")?,
        last_seen_at: row.try_get("last_seen_at")?,
        created_at: row.try_get("created_at")?,
    })
}

fn parse_source_kind(value: &str) -> Result<ExtensionLoadSourceKind> {
    match value {
        "project_mcp" => Ok(ExtensionLoadSourceKind::ProjectMcp),
        "project_skill" => Ok(ExtensionLoadSourceKind::ProjectSkill),
        _ => Err(Error::InvalidData(format!(
            "unknown extension source kind: {value}"
        ))),
    }
}

fn parse_phase(value: &str) -> Result<ExtensionLoadPhase> {
    match value {
        "placeholder_resolution" => Ok(ExtensionLoadPhase::PlaceholderResolution),
        "process_start" => Ok(ExtensionLoadPhase::ProcessStart),
        "transport" => Ok(ExtensionLoadPhase::Transport),
        "handshake" => Ok(ExtensionLoadPhase::Handshake),
        "tools_list" => Ok(ExtensionLoadPhase::ToolsList),
        "frontmatter" => Ok(ExtensionLoadPhase::Frontmatter),
        "missing_file" => Ok(ExtensionLoadPhase::MissingFile),
        "manifest_drift" => Ok(ExtensionLoadPhase::ManifestDrift),
        _ => Err(Error::InvalidData(format!(
            "unknown extension load phase: {value}"
        ))),
    }
}

fn parse_severity(value: &str) -> Result<ExtensionLoadSeverity> {
    match value {
        "warning" => Ok(ExtensionLoadSeverity::Warning),
        "error" => Ok(ExtensionLoadSeverity::Error),
        _ => Err(Error::InvalidData(format!(
            "unknown extension severity: {value}"
        ))),
    }
}

fn parse_remedy_code(value: &str) -> Result<ExtensionLoadRemedyCode> {
    match value {
        "check_placeholder" => Ok(ExtensionLoadRemedyCode::CheckPlaceholder),
        "check_command" => Ok(ExtensionLoadRemedyCode::CheckCommand),
        "check_transport" => Ok(ExtensionLoadRemedyCode::CheckTransport),
        "check_server" => Ok(ExtensionLoadRemedyCode::CheckServer),
        "check_skill_frontmatter" => Ok(ExtensionLoadRemedyCode::CheckSkillFrontmatter),
        "restore_skill_file" => Ok(ExtensionLoadRemedyCode::RestoreSkillFile),
        "update_skill_manifest" => Ok(ExtensionLoadRemedyCode::UpdateSkillManifest),
        _ => Err(Error::InvalidData(format!(
            "unknown extension remedy code: {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::test_support::{
        UsageTestSessionSeed, UsageTestTaskSeed, seed_project, seed_session_row_with_id,
        seed_task_row,
    };

    fn input(
        project_id: String,
        session_id: String,
        task_id: String,
        attempt: &str,
    ) -> InsertExtensionLoadDiagnostic {
        InsertExtensionLoadDiagnostic {
            project_id,
            task_id: Some(task_id),
            session_id: Some(session_id),
            load_attempt_id: attempt.to_owned(),
            source_kind: ExtensionLoadSourceKind::ProjectMcp,
            source_key: "search".to_owned(),
            phase: ExtensionLoadPhase::ToolsList,
            severity: ExtensionLoadSeverity::Error,
            summary: "tools/list returned invalid JSON".to_owned(),
            summary_fingerprint: "a".repeat(64),
            remedy_code: ExtensionLoadRemedyCode::CheckServer,
            remedy: "Check the MCP server health.".to_owned(),
            first_seen_at: "2026-07-13T10:00:00.000Z".to_owned(),
            last_seen_at: "2026-07-13T10:00:00.000Z".to_owned(),
            created_at: "2026-07-13T10:00:00.000Z".to_owned(),
        }
    }

    async fn fixture() -> (Database, String, String, String) {
        let db = Database::open_in_memory().expect("database");
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id, "extension-diagnostic-test").await;
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
                started_at: "2026-07-13T10:00:00.000Z",
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
        (db, project_id, session_id, task_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_dedupes_within_attempt_and_splits_later_attempts() {
        let (db, project_id, session_id, task_id) = fixture().await;
        let repo = ExtensionLoadDiagnosticRepository::new(db);
        let first = repo
            .insert_or_increment(input(
                project_id.clone(),
                session_id.clone(),
                task_id.clone(),
                "attempt-1",
            ))
            .await
            .unwrap();
        let retry = repo
            .insert_or_increment(input(
                project_id.clone(),
                session_id.clone(),
                task_id.clone(),
                "attempt-1",
            ))
            .await
            .unwrap();
        let later = repo
            .insert_or_increment(input(
                project_id.clone(),
                session_id.clone(),
                task_id.clone(),
                "attempt-2",
            ))
            .await
            .unwrap();
        let mut distinct_source = input(project_id, session_id, task_id, "attempt-1");
        distinct_source.source_kind = ExtensionLoadSourceKind::ProjectSkill;
        distinct_source.source_key = "search-skill".to_owned();
        distinct_source.summary_fingerprint = "b".repeat(64);
        let distinct = repo.insert_or_increment(distinct_source).await.unwrap();

        assert_eq!(first.diagnostic_id, retry.diagnostic_id);
        assert_eq!(retry.occurrence_count, 2);
        assert_ne!(first.diagnostic_id, later.diagnostic_id);
        assert_eq!(later.occurrence_count, 1);
        assert_ne!(first.diagnostic_id, distinct.diagnostic_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_lists_are_project_owned_and_ordered() {
        let (db, project_id, session_id, task_id) = fixture().await;
        let repo = ExtensionLoadDiagnosticRepository::new(db.clone());
        let mut warning = input(
            project_id.clone(),
            session_id.clone(),
            task_id.clone(),
            "attempt-1",
        );
        warning.source_key = "alpha".to_owned();
        warning.severity = ExtensionLoadSeverity::Warning;
        warning.summary_fingerprint = "b".repeat(64);
        repo.insert_or_increment(warning).await.unwrap();
        repo.insert_or_increment(input(
            project_id.clone(),
            session_id.clone(),
            task_id.clone(),
            "attempt-1",
        ))
        .await
        .unwrap();

        let by_session = repo
            .list_for_session(&project_id, &session_id)
            .await
            .unwrap();
        assert_eq!(by_session.len(), 2);
        assert_eq!(by_session[0].severity, ExtensionLoadSeverity::Error);
        assert_eq!(
            repo.list_for_task(&project_id, &task_id)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            repo.list_for_load_attempt(&project_id, "attempt-1")
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            repo.list_for_session(&uuid::Uuid::now_v7().to_string(), &session_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            repo.list_for_load_attempt(&project_id, "missing")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
