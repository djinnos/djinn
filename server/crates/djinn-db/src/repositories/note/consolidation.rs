use djinn_core::models::{ConsolidatedNoteProvenance, ConsolidationRunMetric};

use crate::Database;
use crate::error::{DbError as Error, DbResult as Result};

pub struct CreateConsolidationRunMetric<'a> {
    pub project_id: &'a str,
    pub note_type: &'a str,
    pub status: &'a str,
    pub scanned_note_count: i64,
    pub candidate_cluster_count: i64,
    pub consolidated_cluster_count: i64,
    pub consolidated_note_count: i64,
    pub source_note_count: i64,
    pub started_at: &'a str,
    pub completed_at: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

pub struct NoteConsolidationRepository {
    db: Database,
}

impl NoteConsolidationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn add_provenance(
        &self,
        note_id: &str,
        session_id: &str,
    ) -> Result<ConsolidatedNoteProvenance> {
        self.db.ensure_initialized().await?;

        sqlx::query(
            "INSERT INTO consolidated_note_provenance (note_id, session_id)
             VALUES (?1, ?2)",
        )
        .bind(note_id)
        .bind(session_id)
        .execute(self.db.pool())
        .await?;

        self.get_provenance_entry(note_id, session_id).await
    }

    pub async fn list_provenance(&self, note_id: &str) -> Result<Vec<ConsolidatedNoteProvenance>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, ConsolidatedNoteProvenance>(
            "SELECT note_id, session_id, created_at
             FROM consolidated_note_provenance
             WHERE note_id = ?1
             ORDER BY created_at ASC, session_id ASC",
        )
        .bind(note_id)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    pub async fn create_run_metric(
        &self,
        params: CreateConsolidationRunMetric<'_>,
    ) -> Result<ConsolidationRunMetric> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();

        sqlx::query(
            "INSERT INTO consolidation_run_metrics (
                id, project_id, note_type, status,
                scanned_note_count, candidate_cluster_count,
                consolidated_cluster_count, consolidated_note_count,
                source_note_count, started_at, completed_at, error_message
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(&id)
        .bind(params.project_id)
        .bind(params.note_type)
        .bind(params.status)
        .bind(params.scanned_note_count)
        .bind(params.candidate_cluster_count)
        .bind(params.consolidated_cluster_count)
        .bind(params.consolidated_note_count)
        .bind(params.source_note_count)
        .bind(params.started_at)
        .bind(params.completed_at)
        .bind(params.error_message)
        .execute(self.db.pool())
        .await?;

        self.get_run_metric(&id).await
    }

    pub async fn list_run_metrics(
        &self,
        project_id: &str,
        note_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConsolidationRunMetric>> {
        self.db.ensure_initialized().await?;
        let note_type = note_type.unwrap_or("");
        let limit = limit as i64;

        sqlx::query_as::<_, ConsolidationRunMetric>(
            "SELECT id, project_id, note_type, status,
                    scanned_note_count, candidate_cluster_count,
                    consolidated_cluster_count, consolidated_note_count,
                    source_note_count, started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE project_id = ?1
               AND (?2 = '' OR note_type = ?2)
             ORDER BY started_at DESC, id DESC
             LIMIT ?3",
        )
        .bind(project_id)
        .bind(note_type)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    async fn get_provenance_entry(
        &self,
        note_id: &str,
        session_id: &str,
    ) -> Result<ConsolidatedNoteProvenance> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, ConsolidatedNoteProvenance>(
            "SELECT note_id, session_id, created_at
             FROM consolidated_note_provenance
             WHERE note_id = ?1 AND session_id = ?2",
        )
        .bind(note_id)
        .bind(session_id)
        .fetch_one(self.db.pool())
        .await
        .map_err(|err| match err {
            sqlx::Error::RowNotFound => Error::InvalidData(format!(
                "consolidated provenance not found for note {note_id} and session {session_id}"
            )),
            other => other.into(),
        })
    }

    async fn get_run_metric(&self, id: &str) -> Result<ConsolidationRunMetric> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, ConsolidationRunMetric>(
            "SELECT id, project_id, note_type, status,
                    scanned_note_count, candidate_cluster_count,
                    consolidated_cluster_count, consolidated_note_count,
                    source_note_count, started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await
        .map_err(|err| match err {
            sqlx::Error::RowNotFound => {
                Error::InvalidData(format!("consolidation run metric not found: {id}"))
            }
            other => other.into(),
        })
    }
}
