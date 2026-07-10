// djinn:allow-oversize
//! Audit sampler repository: durable storage for the independent audit-sampler
//! and false-negative ledger (epic ihf1).
//!
//! Provides insert/upsert for merged-change facts, eligible-window queries,
//! sealed frame revision creation, selection/audit-item recording, and
//! audit outcome/query operations.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;

// ── Domain types ──────────────────────────────────────────────────────────────

/// Which stratum a merged change belongs to. Stratum (b) — autonomous releases
/// — samples at a higher initial rate to directly audit autonomous release
/// authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditStratum {
    /// Normal merged changes with no tripwire finding.
    UnflaggedMerged,
    /// Merged changes whose tripwire holds were released by the autonomous
    /// planner/arbiter adjudication path (released_by_role != human).
    AutonomousRelease,
}

impl AuditStratum {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnflaggedMerged => "unflagged_merged",
            Self::AutonomousRelease => "autonomous_release",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unflagged_merged" => Some(Self::UnflaggedMerged),
            "autonomous_release" => Some(Self::AutonomousRelease),
            _ => None,
        }
    }
}

/// Audit outcome for a selected item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcomeKind {
    /// No false negative found.
    Clean,
    /// A false negative was found.
    Miss,
}

impl AuditOutcomeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Miss => "miss",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "clean" => Some(Self::Clean),
            "miss" => Some(Self::Miss),
            _ => None,
        }
    }
}

// ── Row types ─────────────────────────────────────────────────────────────────

/// A merged-change fact stored in `audit_merged_changes`.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MergedChangeRow {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub pr_number: Option<i64>,
    pub head_sha: Option<String>,
    pub merge_commit_sha: String,
    pub merged_at: String,
    pub gate_outcome: String,
    pub gate_provenance: Option<serde_json::Value>,
    pub release_provenance: Option<serde_json::Value>,
    pub stratum: String,
    pub excluded: bool,
    pub exclusion_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl MergedChangeRow {
    /// Parse the `stratum` column into the typed enum.
    pub fn stratum_enum(&self) -> Option<AuditStratum> {
        AuditStratum::parse(&self.stratum)
    }
}

/// A sample policy revision.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct SamplePolicyRow {
    pub id: String,
    pub project_id: String,
    pub revision: i32,
    pub policy_json: serde_json::Value,
    pub created_at: String,
}

/// A sealed sample frame revision.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct SampleFrameRow {
    pub id: String,
    pub project_id: String,
    pub policy_id: String,
    pub window_start: String,
    pub window_end: String,
    pub revision: i32,
    pub eligible_change_ids: serde_json::Value,
    pub content_hash: Option<String>,
    pub exclusion_counts: serde_json::Value,
    pub exclusion_reasons: serde_json::Value,
    pub superseded_by_id: Option<String>,
    pub sealed_at: String,
    pub created_at: String,
}

/// A selection / audit-item record.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct SelectionRow {
    pub id: String,
    pub frame_id: String,
    pub merged_change_id: String,
    pub stratum: String,
    pub selected_position: i32,
    pub algorithm: String,
    pub seed_commitment: String,
    pub seed_reveal: Option<String>,
    pub replay_data: serde_json::Value,
    pub audit_task_id: Option<String>,
    pub created_at: String,
}

impl SelectionRow {
    pub fn stratum_enum(&self) -> Option<AuditStratum> {
        AuditStratum::parse(&self.stratum)
    }
}

/// An audit outcome / false-negative ledger entry.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditOutcomeRow {
    pub id: String,
    pub selection_id: String,
    pub outcome: String,
    pub miss_category: Option<String>,
    pub miss_severity: Option<String>,
    pub requires_rule_update: bool,
    pub notes: Option<String>,
    pub recorded_at: String,
    pub created_at: String,
}

impl AuditOutcomeRow {
    pub fn outcome_enum(&self) -> Option<AuditOutcomeKind> {
        AuditOutcomeKind::parse(&self.outcome)
    }
}

/// Summary row for audit outcome reports, joining selection + merged change data.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditOutcomeReportRow {
    pub outcome_id: String,
    pub selection_id: String,
    pub outcome: String,
    pub miss_category: Option<String>,
    pub miss_severity: Option<String>,
    pub requires_rule_update: bool,
    pub notes: Option<String>,
    pub recorded_at: String,
    pub stratum: String,
    pub merge_commit_sha: String,
    pub project_id: String,
}

// ── Input types ───────────────────────────────────────────────────────────────

/// Parameters for upserting a merged-change fact.
pub struct UpsertMergedChangeParams<'a> {
    pub project_id: &'a str,
    pub task_id: Option<&'a str>,
    pub pr_number: Option<i64>,
    pub head_sha: Option<&'a str>,
    pub merge_commit_sha: &'a str,
    pub merged_at: &'a str,
    pub gate_outcome: &'a str,
    pub gate_provenance: Option<&'a serde_json::Value>,
    pub release_provenance: Option<&'a serde_json::Value>,
    pub stratum: AuditStratum,
    pub excluded: bool,
    pub exclusion_reason: Option<&'a str>,
}

/// Parameters for creating a sample policy revision.
pub struct CreateSamplePolicyParams<'a> {
    pub project_id: &'a str,
    pub revision: i32,
    pub policy_json: &'a serde_json::Value,
}

/// Parameters for creating a sealed sample frame revision.
pub struct CreateSampleFrameParams<'a> {
    pub project_id: &'a str,
    pub policy_id: &'a str,
    pub window_start: &'a str,
    pub window_end: &'a str,
    pub revision: i32,
    pub eligible_change_ids: &'a serde_json::Value,
    pub content_hash: Option<&'a str>,
    pub exclusion_counts: &'a serde_json::Value,
    pub exclusion_reasons: &'a serde_json::Value,
    pub sealed_at: &'a str,
}

/// Parameters for recording a selection / audit-item.
pub struct CreateSelectionParams<'a> {
    pub frame_id: &'a str,
    pub merged_change_id: &'a str,
    pub stratum: AuditStratum,
    pub selected_position: i32,
    pub algorithm: &'a str,
    pub seed_commitment: &'a str,
    pub seed_reveal: Option<&'a str>,
    pub replay_data: &'a serde_json::Value,
    pub audit_task_id: Option<&'a str>,
}

/// Parameters for recording an audit outcome.
pub struct RecordOutcomeParams<'a> {
    pub selection_id: &'a str,
    pub outcome: AuditOutcomeKind,
    pub miss_category: Option<&'a str>,
    pub miss_severity: Option<&'a str>,
    pub requires_rule_update: bool,
    pub notes: Option<&'a str>,
}

// ── Repository ────────────────────────────────────────────────────────────────

pub struct AuditSamplerRepository {
    db: Database,
}

impl AuditSamplerRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    // ── Merged-change facts ──────────────────────────────────────────────

    /// Upsert a merged-change fact. If `merge_commit_sha` already exists,
    /// the row is updated with the latest provenance / stratum / exclusion
    /// data.
    pub async fn upsert_merged_change(
        &self,
        params: UpsertMergedChangeParams<'_>,
    ) -> Result<MergedChangeRow> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO audit_merged_changes (
                    id, project_id, task_id, pr_number, head_sha,
                    merge_commit_sha, merged_at,
                    gate_outcome, gate_provenance, release_provenance,
                    stratum, excluded, exclusion_reason
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                ON CONFLICT (merge_commit_sha) DO UPDATE SET
                    project_id = EXCLUDED.project_id,
                    task_id = EXCLUDED.task_id,
                    pr_number = EXCLUDED.pr_number,
                    head_sha = EXCLUDED.head_sha,
                    merged_at = EXCLUDED.merged_at,
                    gate_outcome = EXCLUDED.gate_outcome,
                    gate_provenance = EXCLUDED.gate_provenance,
                    release_provenance = EXCLUDED.release_provenance,
                    stratum = EXCLUDED.stratum,
                    excluded = EXCLUDED.excluded,
                    exclusion_reason = EXCLUDED.exclusion_reason,
                    updated_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
        )
        .bind(&id)
        .bind(params.project_id)
        .bind(params.task_id)
        .bind(params.pr_number)
        .bind(params.head_sha)
        .bind(params.merge_commit_sha)
        .bind(params.merged_at)
        .bind(params.gate_outcome)
        .bind(params.gate_provenance)
        .bind(params.release_provenance)
        .bind(params.stratum.as_str())
        .bind(params.excluded)
        .bind(params.exclusion_reason)
        .execute(self.db.pool())
        .await?;

        // Fetch back the row (id may differ on update since we generate a
        // new UUID on insert but the ON CONFLICT keeps the original id).
        Ok(self
            .get_merged_change_by_sha(params.merge_commit_sha)
            .await?
            .expect("row just upserted must exist"))
    }

    /// Read a merged-change fact by merge commit SHA.
    pub async fn get_merged_change_by_sha(
        &self,
        merge_commit_sha: &str,
    ) -> Result<Option<MergedChangeRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, MergedChangeRow>(MERGED_CHANGE_BY_SHA)
            .bind(merge_commit_sha)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// Read a merged-change fact by id.
    pub async fn get_merged_change_by_id(&self, id: &str) -> Result<Option<MergedChangeRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, MergedChangeRow>(MERGED_CHANGE_BY_ID)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// Read eligible (non-excluded) merged-change facts within a settled
    /// time window, ordered by merged_at then merge_commit_sha for
    /// deterministic frame construction.
    pub async fn list_eligible_changes_in_window(
        &self,
        project_id: &str,
        window_start: &str,
        window_end: &str,
    ) -> Result<Vec<MergedChangeRow>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, MergedChangeRow>(ELIGIBLE_CHANGES_IN_WINDOW)
                .bind(project_id)
                .bind(window_start)
                .bind(window_end)
                .fetch_all(self.db.pool())
                .await?,
        )
    }

    // ── Sample policies ──────────────────────────────────────────────────

    /// Create a new sample policy revision.
    pub async fn create_sample_policy(
        &self,
        params: CreateSamplePolicyParams<'_>,
    ) -> Result<SamplePolicyRow> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO audit_sample_policies (id, project_id, revision, policy_json)
               VALUES ($1, $2, $3, $4)"#,
        )
        .bind(&id)
        .bind(params.project_id)
        .bind(params.revision)
        .bind(params.policy_json)
        .execute(self.db.pool())
        .await?;

        Ok(self
            .get_sample_policy_by_id(&id)
            .await?
            .expect("row just inserted must exist"))
    }

    /// Read a sample policy by id.
    pub async fn get_sample_policy_by_id(&self, id: &str) -> Result<Option<SamplePolicyRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SamplePolicyRow>(SAMPLE_POLICY_BY_ID)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// Read the latest sample policy for a project.
    pub async fn get_latest_sample_policy(
        &self,
        project_id: &str,
    ) -> Result<Option<SamplePolicyRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SamplePolicyRow>(LATEST_SAMPLE_POLICY)
            .bind(project_id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    // ── Sample frames ────────────────────────────────────────────────────

    /// Create a sealed sample frame revision. The frame is immutable once
    /// created; late corrections produce a new revision with an audit event.
    pub async fn create_sample_frame(
        &self,
        params: CreateSampleFrameParams<'_>,
    ) -> Result<SampleFrameRow> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO audit_sample_frames (
                    id, project_id, policy_id, window_start, window_end,
                    revision, eligible_change_ids, content_hash,
                    exclusion_counts, exclusion_reasons, sealed_at
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#,
        )
        .bind(&id)
        .bind(params.project_id)
        .bind(params.policy_id)
        .bind(params.window_start)
        .bind(params.window_end)
        .bind(params.revision)
        .bind(params.eligible_change_ids)
        .bind(params.content_hash)
        .bind(params.exclusion_counts)
        .bind(params.exclusion_reasons)
        .bind(params.sealed_at)
        .execute(self.db.pool())
        .await?;

        Ok(self
            .get_sample_frame_by_id(&id)
            .await?
            .expect("row just inserted must exist"))
    }

    /// Read a sample frame by id.
    pub async fn get_sample_frame_by_id(&self, id: &str) -> Result<Option<SampleFrameRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SampleFrameRow>(SAMPLE_FRAME_BY_ID)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// Read all frame revisions for a given (project, window), ordered by
    /// revision ascending.
    pub async fn list_sample_frames_in_window(
        &self,
        project_id: &str,
        window_start: &str,
        window_end: &str,
    ) -> Result<Vec<SampleFrameRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SampleFrameRow>(SAMPLE_FRAMES_IN_WINDOW)
            .bind(project_id)
            .bind(window_start)
            .bind(window_end)
            .fetch_all(self.db.pool())
            .await?)
    }

    /// Link a frame to its superseding revision.
    pub async fn mark_frame_superseded(
        &self,
        frame_id: &str,
        superseded_by_id: &str,
    ) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(FRAME_MARK_SUPERSEDED)
            .bind(superseded_by_id)
            .bind(frame_id)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    // ── Selections ───────────────────────────────────────────────────────

    /// Record a selection / audit-item row.
    pub async fn create_selection(
        &self,
        params: CreateSelectionParams<'_>,
    ) -> Result<SelectionRow> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO audit_selections (
                    id, frame_id, merged_change_id, stratum,
                    selected_position, algorithm,
                    seed_commitment, seed_reveal, replay_data,
                    audit_task_id
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
        )
        .bind(&id)
        .bind(params.frame_id)
        .bind(params.merged_change_id)
        .bind(params.stratum.as_str())
        .bind(params.selected_position)
        .bind(params.algorithm)
        .bind(params.seed_commitment)
        .bind(params.seed_reveal)
        .bind(params.replay_data)
        .bind(params.audit_task_id)
        .execute(self.db.pool())
        .await?;

        Ok(self
            .get_selection_by_id(&id)
            .await?
            .expect("row just inserted must exist"))
    }

    /// Read a selection by id.
    pub async fn get_selection_by_id(&self, id: &str) -> Result<Option<SelectionRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SelectionRow>(SELECTION_BY_ID)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// List all selections for a given frame, ordered by position.
    pub async fn list_selections_for_frame(&self, frame_id: &str) -> Result<Vec<SelectionRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SelectionRow>(SELECTIONS_BY_FRAME)
            .bind(frame_id)
            .fetch_all(self.db.pool())
            .await?)
    }

    /// Link an audit task to an existing selection.
    pub async fn set_selection_audit_task(
        &self,
        selection_id: &str,
        audit_task_id: &str,
    ) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(SET_SELECTION_AUDIT_TASK)
            .bind(audit_task_id)
            .bind(selection_id)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    // ── Audit outcomes ───────────────────────────────────────────────────

    /// Record an audit outcome for a selection. Each selection can have at
    /// most one outcome (unique constraint on selection_id).
    pub async fn record_outcome(&self, params: RecordOutcomeParams<'_>) -> Result<AuditOutcomeRow> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO audit_outcomes (
                    id, selection_id, outcome,
                    miss_category, miss_severity, requires_rule_update, notes
                ) VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
        )
        .bind(&id)
        .bind(params.selection_id)
        .bind(params.outcome.as_str())
        .bind(params.miss_category)
        .bind(params.miss_severity)
        .bind(params.requires_rule_update)
        .bind(params.notes)
        .execute(self.db.pool())
        .await?;

        Ok(self
            .get_outcome_by_id(&id)
            .await?
            .expect("row just inserted must exist"))
    }

    /// Read an audit outcome by id.
    pub async fn get_outcome_by_id(&self, id: &str) -> Result<Option<AuditOutcomeRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, AuditOutcomeRow>(OUTCOME_BY_ID)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// Read the outcome for a specific selection.
    pub async fn get_outcome_for_selection(
        &self,
        selection_id: &str,
    ) -> Result<Option<AuditOutcomeRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, AuditOutcomeRow>(OUTCOME_BY_SELECTION)
            .bind(selection_id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// List audit outcomes for a project, joined with selection and merged-
    /// change data for report generation. Ordered by recorded_at descending.
    pub async fn list_outcomes_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<AuditOutcomeReportRow>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, AuditOutcomeReportRow>(OUTCOMES_FOR_PROJECT)
                .bind(project_id)
                .fetch_all(self.db.pool())
                .await?,
        )
    }
}

// ── SQL constants ─────────────────────────────────────────────────────────────

const MERGED_CHANGE_BY_SHA: &str = r#"
    SELECT id, project_id, task_id, pr_number, head_sha,
           merge_commit_sha, merged_at,
           gate_outcome, gate_provenance, release_provenance,
           stratum, excluded, exclusion_reason,
           created_at, updated_at
    FROM audit_merged_changes
    WHERE merge_commit_sha = $1
"#;

const MERGED_CHANGE_BY_ID: &str = r#"
    SELECT id, project_id, task_id, pr_number, head_sha,
           merge_commit_sha, merged_at,
           gate_outcome, gate_provenance, release_provenance,
           stratum, excluded, exclusion_reason,
           created_at, updated_at
    FROM audit_merged_changes
    WHERE id = $1
"#;

const ELIGIBLE_CHANGES_IN_WINDOW: &str = r#"
    SELECT id, project_id, task_id, pr_number, head_sha,
           merge_commit_sha, merged_at,
           gate_outcome, gate_provenance, release_provenance,
           stratum, excluded, exclusion_reason,
           created_at, updated_at
    FROM audit_merged_changes
    WHERE project_id = $1
      AND merged_at >= $2
      AND merged_at < $3
      AND excluded = FALSE
    ORDER BY merged_at ASC, merge_commit_sha ASC
"#;

const SAMPLE_POLICY_BY_ID: &str = r#"
    SELECT id, project_id, revision, policy_json, created_at
    FROM audit_sample_policies
    WHERE id = $1
"#;

const LATEST_SAMPLE_POLICY: &str = r#"
    SELECT id, project_id, revision, policy_json, created_at
    FROM audit_sample_policies
    WHERE project_id = $1
    ORDER BY revision DESC
    LIMIT 1
"#;

const SAMPLE_FRAME_BY_ID: &str = r#"
    SELECT id, project_id, policy_id, window_start, window_end,
           revision, eligible_change_ids, content_hash,
           exclusion_counts, exclusion_reasons,
           superseded_by_id, sealed_at, created_at
    FROM audit_sample_frames
    WHERE id = $1
"#;

const SAMPLE_FRAMES_IN_WINDOW: &str = r#"
    SELECT id, project_id, policy_id, window_start, window_end,
           revision, eligible_change_ids, content_hash,
           exclusion_counts, exclusion_reasons,
           superseded_by_id, sealed_at, created_at
    FROM audit_sample_frames
    WHERE project_id = $1
      AND window_start = $2
      AND window_end = $3
    ORDER BY revision ASC
"#;

const FRAME_MARK_SUPERSEDED: &str = r#"
    UPDATE audit_sample_frames
    SET superseded_by_id = $1
    WHERE id = $2 AND superseded_by_id IS NULL
"#;

const SELECTION_BY_ID: &str = r#"
    SELECT id, frame_id, merged_change_id, stratum,
           selected_position, algorithm,
           seed_commitment, seed_reveal, replay_data,
           audit_task_id, created_at
    FROM audit_selections
    WHERE id = $1
"#;

const SELECTIONS_BY_FRAME: &str = r#"
    SELECT id, frame_id, merged_change_id, stratum,
           selected_position, algorithm,
           seed_commitment, seed_reveal, replay_data,
           audit_task_id, created_at
    FROM audit_selections
    WHERE frame_id = $1
    ORDER BY selected_position ASC
"#;

const SET_SELECTION_AUDIT_TASK: &str = r#"
    UPDATE audit_selections
    SET audit_task_id = $1
    WHERE id = $2
"#;

const OUTCOME_BY_ID: &str = r#"
    SELECT id, selection_id, outcome,
           miss_category, miss_severity, requires_rule_update,
           notes, recorded_at, created_at
    FROM audit_outcomes
    WHERE id = $1
"#;

const OUTCOME_BY_SELECTION: &str = r#"
    SELECT id, selection_id, outcome,
           miss_category, miss_severity, requires_rule_update,
           notes, recorded_at, created_at
    FROM audit_outcomes
    WHERE selection_id = $1
"#;

const OUTCOMES_FOR_PROJECT: &str = r#"
    SELECT
        ao.id AS outcome_id,
        ao.selection_id,
        ao.outcome,
        ao.miss_category,
        ao.miss_severity,
        ao.requires_rule_update,
        ao.notes,
        ao.recorded_at,
        asel.stratum,
        amc.merge_commit_sha,
        amc.project_id
    FROM audit_outcomes ao
    JOIN audit_selections asel ON asel.id = ao.selection_id
    JOIN audit_merged_changes amc ON amc.id = asel.merged_change_id
    WHERE amc.project_id = $1
    ORDER BY ao.recorded_at DESC
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::database::Database;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Seed a project so FK constraints pass.
    async fn seed_project(db: &Database, project_id: &str) {
        db.ensure_initialized().await.unwrap();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo)
             VALUES ($1, $2, 'test-owner', $2)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(project_id)
        .bind(format!("proj-{project_id}"))
        .execute(db.pool())
        .await
        .unwrap();
    }

    /// Create a sample policy for the given project and return its id.
    async fn seed_policy(
        repo: &AuditSamplerRepository,
        project_id: &str,
        revision: i32,
    ) -> SamplePolicyRow {
        repo.create_sample_policy(CreateSamplePolicyParams {
            project_id,
            revision,
            policy_json: &json!({"unflagged_rate": 0.02, "autonomous_rate": 0.10}),
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn migration_creates_all_tables() {
        let db = test_db();
        // ensure_initialized runs all migrations including 102.
        db.ensure_initialized().await.unwrap();

        // Verify each table exists by counting rows (0 is fine).
        for table in &[
            "audit_merged_changes",
            "audit_sample_policies",
            "audit_sample_frames",
            "audit_selections",
            "audit_outcomes",
        ] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*)::bigint FROM {table}"))
                .fetch_one(db.pool())
                .await
                .unwrap_or_else(|e| panic!("table {table} must exist: {e}"));
            assert_eq!(count, 0, "fresh table {table} should be empty");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_merged_change_creates_row() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);

        let row = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &project_id,
                task_id: Some("task-abc"),
                pr_number: Some(42),
                head_sha: Some("aaa111"),
                merge_commit_sha: "bbb222",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: "pass",
                gate_provenance: Some(&json!({"tripwire": "none"})),
                release_provenance: None,
                stratum: AuditStratum::UnflaggedMerged,
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        assert_eq!(row.project_id, project_id);
        assert_eq!(row.task_id.as_deref(), Some("task-abc"));
        assert_eq!(row.pr_number, Some(42));
        assert_eq!(row.merge_commit_sha, "bbb222");
        assert_eq!(row.stratum, "unflagged_merged");
        assert!(!row.excluded);
        assert!(row.gate_provenance.is_some());
        assert!(row.release_provenance.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_merged_change_idempotent_on_conflict() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);

        // First insert
        let row1 = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &project_id,
                task_id: None,
                pr_number: None,
                head_sha: None,
                merge_commit_sha: "sha-dup-test",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: "pass",
                gate_provenance: None,
                release_provenance: None,
                stratum: AuditStratum::UnflaggedMerged,
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        // Upsert with same SHA but different stratum/provenance
        let row2 = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &project_id,
                task_id: Some("task-999"),
                pr_number: Some(7),
                head_sha: Some("new-head"),
                merge_commit_sha: "sha-dup-test",
                merged_at: "2026-07-01T12:00:00Z",
                gate_outcome: "released_by_arbiter",
                gate_provenance: None,
                release_provenance: Some(&json!({"released_by": "arbiter"})),
                stratum: AuditStratum::AutonomousRelease,
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        // Same underlying row (same id from first insert)
        assert_eq!(row1.id, row2.id);
        // Updated fields
        assert_eq!(row2.task_id.as_deref(), Some("task-999"));
        assert_eq!(row2.pr_number, Some(7));
        assert_eq!(row2.stratum, "autonomous_release");
        assert_eq!(row2.gate_outcome, "released_by_arbiter");
        assert!(row2.release_provenance.is_some());
        assert!(row2.updated_at >= row1.updated_at);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_eligible_changes_in_window_sorted() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);

        // Insert 3 changes, one excluded
        for (sha, time, excluded) in &[
            ("sha-b", "2026-07-02T00:00:00Z", false),
            ("sha-a", "2026-07-01T00:00:00Z", false),
            ("sha-c", "2026-07-03T00:00:00Z", true),
        ] {
            repo.upsert_merged_change(UpsertMergedChangeParams {
                project_id: &project_id,
                task_id: None,
                pr_number: None,
                head_sha: None,
                merge_commit_sha: sha,
                merged_at: time,
                gate_outcome: "pass",
                gate_provenance: None,
                release_provenance: None,
                stratum: AuditStratum::UnflaggedMerged,
                excluded: *excluded,
                exclusion_reason: if *excluded {
                    Some("outside_window")
                } else {
                    None
                },
            })
            .await
            .unwrap();
        }

        let eligible = repo
            .list_eligible_changes_in_window(
                &project_id,
                "2026-07-01T00:00:00Z",
                "2026-07-04T00:00:00Z",
            )
            .await
            .unwrap();

        assert_eq!(eligible.len(), 2, "excluded change should not appear");
        assert_eq!(eligible[0].merge_commit_sha, "sha-a");
        assert_eq!(eligible[1].merge_commit_sha, "sha-b");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_read_sample_policy() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);

        let p1 = seed_policy(&repo, &project_id, 1).await;
        let p2 = seed_policy(&repo, &project_id, 2).await;

        assert_eq!(p1.revision, 1);
        assert_eq!(p2.revision, 2);

        // Latest should be revision 2
        let latest = repo
            .get_latest_sample_policy(&project_id)
            .await
            .unwrap()
            .expect("must have latest");
        assert_eq!(latest.revision, 2);

        // By id
        let fetched = repo
            .get_sample_policy_by_id(&p1.id)
            .await
            .unwrap()
            .expect("must exist");
        assert_eq!(fetched.revision, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_read_sample_frame() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);
        let policy = seed_policy(&repo, &project_id, 1).await;

        let eligible_ids = json!(["change-1", "change-2", "change-3"]);
        let frame = repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id: &project_id,
                policy_id: &policy.id,
                window_start: "2026-06-24T00:00:00Z",
                window_end: "2026-07-01T00:00:00Z",
                revision: 1,
                eligible_change_ids: &eligible_ids,
                content_hash: Some(
                    "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233",
                ),
                exclusion_counts: &json!({"outside_window": 2}),
                exclusion_reasons: &json!(["outside_window"]),
                sealed_at: "2026-07-01T00:05:00Z",
            })
            .await
            .unwrap();

        assert_eq!(frame.project_id, project_id);
        assert_eq!(frame.policy_id, policy.id);
        assert_eq!(frame.revision, 1);
        assert_eq!(frame.window_start, "2026-06-24T00:00:00Z");
        assert!(frame.content_hash.is_some());
        assert!(frame.superseded_by_id.is_none());

        // List frames in window
        let frames = repo
            .list_sample_frames_in_window(
                &project_id,
                "2026-06-24T00:00:00Z",
                "2026-07-01T00:00:00Z",
            )
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_frame_superseded_works() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);
        let policy = seed_policy(&repo, &project_id, 1).await;

        let frame1 = repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id: &project_id,
                policy_id: &policy.id,
                window_start: "2026-06-24T00:00:00Z",
                window_end: "2026-07-01T00:00:00Z",
                revision: 1,
                eligible_change_ids: &json!(["a"]),
                content_hash: None,
                exclusion_counts: &json!({}),
                exclusion_reasons: &json!([]),
                sealed_at: "2026-07-01T00:05:00Z",
            })
            .await
            .unwrap();

        let frame2 = repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id: &project_id,
                policy_id: &policy.id,
                window_start: "2026-06-24T00:00:00Z",
                window_end: "2026-07-01T00:00:00Z",
                revision: 2,
                eligible_change_ids: &json!(["a", "b"]),
                content_hash: None,
                exclusion_counts: &json!({}),
                exclusion_reasons: &json!([]),
                sealed_at: "2026-07-01T01:00:00Z",
            })
            .await
            .unwrap();

        let updated = repo
            .mark_frame_superseded(&frame1.id, &frame2.id)
            .await
            .unwrap();
        assert!(updated);

        let fetched = repo
            .get_sample_frame_by_id(&frame1.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.superseded_by_id.as_deref(), Some(&*frame2.id));

        // Idempotent — second call should return false (already superseded)
        let again = repo
            .mark_frame_superseded(&frame1.id, &frame2.id)
            .await
            .unwrap();
        assert!(!again, "already superseded should not update again");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_read_selection() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);
        let policy = seed_policy(&repo, &project_id, 1).await;

        // Need a merged change and a frame first
        let change = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &project_id,
                task_id: None,
                pr_number: Some(10),
                head_sha: Some("head10"),
                merge_commit_sha: "merge10",
                merged_at: "2026-06-28T00:00:00Z",
                gate_outcome: "pass",
                gate_provenance: None,
                release_provenance: None,
                stratum: AuditStratum::UnflaggedMerged,
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        let frame = repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id: &project_id,
                policy_id: &policy.id,
                window_start: "2026-06-24T00:00:00Z",
                window_end: "2026-07-01T00:00:00Z",
                revision: 1,
                eligible_change_ids: &json!([&change.id]),
                content_hash: None,
                exclusion_counts: &json!({}),
                exclusion_reasons: &json!([]),
                sealed_at: "2026-07-01T00:05:00Z",
            })
            .await
            .unwrap();

        let sel = repo
            .create_selection(CreateSelectionParams {
                frame_id: &frame.id,
                merged_change_id: &change.id,
                stratum: AuditStratum::UnflaggedMerged,
                selected_position: 0,
                algorithm: "hmac-sha256-counter-v1",
                seed_commitment: &"aa".repeat(32),
                seed_reveal: None,
                replay_data: &json!({"counter_seq": [0]}),
                audit_task_id: None,
            })
            .await
            .unwrap();

        assert_eq!(sel.frame_id, frame.id);
        assert_eq!(sel.merged_change_id, change.id);
        assert_eq!(sel.stratum, "unflagged_merged");
        assert_eq!(sel.selected_position, 0);
        assert_eq!(sel.algorithm, "hmac-sha256-counter-v1");
        assert!(sel.audit_task_id.is_none());

        // List by frame
        let selections = repo.list_selections_for_frame(&frame.id).await.unwrap();
        assert_eq!(selections.len(), 1);

        // Set audit task id
        let updated = repo
            .set_selection_audit_task(&sel.id, "audit-task-123")
            .await
            .unwrap();
        assert!(updated);

        let fetched = repo.get_selection_by_id(&sel.id).await.unwrap().unwrap();
        assert_eq!(fetched.audit_task_id.as_deref(), Some("audit-task-123"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_and_query_outcome() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id).await;
        let repo = AuditSamplerRepository::new(db);
        let policy = seed_policy(&repo, &project_id, 1).await;

        let change = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &project_id,
                task_id: None,
                pr_number: Some(5),
                head_sha: Some("head5"),
                merge_commit_sha: "merge5",
                merged_at: "2026-06-28T00:00:00Z",
                gate_outcome: "released_by_arbiter",
                gate_provenance: Some(&json!({"tripwire": "finding-1"})),
                release_provenance: Some(&json!({"released_by": "arbiter"})),
                stratum: AuditStratum::AutonomousRelease,
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        let frame = repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id: &project_id,
                policy_id: &policy.id,
                window_start: "2026-06-24T00:00:00Z",
                window_end: "2026-07-01T00:00:00Z",
                revision: 1,
                eligible_change_ids: &json!([&change.id]),
                content_hash: None,
                exclusion_counts: &json!({}),
                exclusion_reasons: &json!([]),
                sealed_at: "2026-07-01T00:05:00Z",
            })
            .await
            .unwrap();

        let sel = repo
            .create_selection(CreateSelectionParams {
                frame_id: &frame.id,
                merged_change_id: &change.id,
                stratum: AuditStratum::AutonomousRelease,
                selected_position: 0,
                algorithm: "hmac-sha256-counter-v1",
                seed_commitment: &"bb".repeat(32),
                seed_reveal: Some(&"cc".repeat(32)),
                replay_data: &json!({}),
                audit_task_id: None,
            })
            .await
            .unwrap();

        // Record a miss outcome
        let outcome = repo
            .record_outcome(RecordOutcomeParams {
                selection_id: &sel.id,
                outcome: AuditOutcomeKind::Miss,
                miss_category: Some("missed_security_finding"),
                miss_severity: Some("high"),
                requires_rule_update: true,
                notes: Some("Tripwire should have caught this"),
            })
            .await
            .unwrap();

        assert_eq!(outcome.outcome, "miss");
        assert_eq!(
            outcome.miss_category.as_deref(),
            Some("missed_security_finding")
        );
        assert_eq!(outcome.miss_severity.as_deref(), Some("high"));
        assert!(outcome.requires_rule_update);

        // Query by selection
        let fetched = repo
            .get_outcome_for_selection(&sel.id)
            .await
            .unwrap()
            .expect("must exist");
        assert_eq!(fetched.id, outcome.id);
        assert_eq!(fetched.outcome_enum(), Some(AuditOutcomeKind::Miss));

        // Report query
        let report = repo.list_outcomes_for_project(&project_id).await.unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].stratum, "autonomous_release");
        assert_eq!(report[0].merge_commit_sha, "merge5");
        assert_eq!(report[0].project_id, project_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_stratum_round_trip() {
        for s in &[
            AuditStratum::UnflaggedMerged,
            AuditStratum::AutonomousRelease,
        ] {
            let as_str = s.as_str();
            let parsed = AuditStratum::parse(as_str).unwrap();
            assert_eq!(*s, parsed);
        }
        assert!(AuditStratum::parse("bogus").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_outcome_kind_round_trip() {
        for k in &[AuditOutcomeKind::Clean, AuditOutcomeKind::Miss] {
            let as_str = k.as_str();
            let parsed = AuditOutcomeKind::parse(as_str).unwrap();
            assert_eq!(*k, parsed);
        }
        assert!(AuditOutcomeKind::parse("bogus").is_none());
    }
}
