//! Typed, transactional persistence for Agent Readiness runs.
use crate::{Error, Result, database::Database};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ReadinessRunRow {
    pub id: String,
    pub project_id: String,
    pub idempotency_key: String,
    pub status: String,
    pub repository_snapshot: String,
    pub skill_name: String,
    pub skill_version: String,
    pub expected_area_count: Option<i32>,
    pub created_at: String,
    pub completed_at: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ReadinessCompositionAreaRow {
    pub id: String,
    pub run_id: String,
    pub area_key: String,
    pub composition: serde_json::Value,
    pub path_scopes: serde_json::Value,
    pub frozen_at: String,
    pub status: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ReadinessAreaAttemptRow {
    pub id: String,
    pub run_id: String,
    pub area_id: String,
    pub attempt_number: i32,
    pub correlation_key: String,
    pub status: String,
    pub payload_digest: Option<String>,
    pub created_at: String,
    pub terminal_at: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ReadinessGuardrailFindingRow {
    pub id: String,
    pub run_id: String,
    pub area_id: String,
    pub attempt_id: String,
    pub guardrail_key: String,
    pub severity: String,
    pub accepted: bool,
    pub evidence: serde_json::Value,
    pub created_at: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ReadinessRemediationSuggestionRow {
    pub id: String,
    pub run_id: String,
    pub dedupe_key: String,
    pub suggestion: serde_json::Value,
    pub created_at: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ReadinessRunEventRow {
    pub id: String,
    pub run_id: String,
    pub event_kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}
#[derive(Clone, Debug)]
pub struct CreateReadinessRun {
    pub project_id: String,
    pub idempotency_key: String,
    pub repository_snapshot: String,
    pub skill_name: String,
    pub skill_version: String,
}
#[derive(Clone, Debug)]
pub struct CreateReadinessCompositionArea {
    pub run_id: String,
    pub area_key: String,
    pub composition: serde_json::Value,
    pub path_scopes: serde_json::Value,
}
#[derive(Clone, Debug)]
pub struct CreateReadinessAreaAttempt {
    pub run_id: String,
    pub area_id: String,
    pub attempt_number: i32,
    pub correlation_key: String,
}
#[derive(Clone, Debug)]
pub struct NewReadinessFinding {
    pub guardrail_key: String,
    pub severity: String,
    pub evidence: serde_json::Value,
}
#[derive(Clone, Debug)]
pub struct NewReadinessSuggestion {
    pub dedupe_key: String,
    pub suggestion: serde_json::Value,
}
#[derive(Clone, Debug)]
pub struct NewReadinessEvent {
    pub event_kind: String,
    pub payload: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ReadinessRunDetail {
    pub run: ReadinessRunRow,
    pub areas: Vec<ReadinessCompositionAreaRow>,
    pub attempts: Vec<ReadinessAreaAttemptRow>,
    pub findings: Vec<ReadinessGuardrailFindingRow>,
    pub suggestions: Vec<ReadinessRemediationSuggestionRow>,
    pub events: Vec<ReadinessRunEventRow>,
}
pub struct ReadinessRepository {
    db: Database,
}
impl ReadinessRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
    pub async fn create_run(&self, i: CreateReadinessRun) -> Result<ReadinessRunRow> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (project_id,idempotency_key) DO UPDATE SET idempotency_key=EXCLUDED.idempotency_key RETURNING id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at").bind(Uuid::now_v7().to_string()).bind(i.project_id).bind(i.idempotency_key).bind(i.repository_snapshot).bind(i.skill_name).bind(i.skill_version).fetch_one(self.db.pool()).await.map_err(Into::into)
    }
    pub async fn create_area(
        &self,
        i: CreateReadinessCompositionArea,
    ) -> Result<ReadinessCompositionAreaRow> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("INSERT INTO readiness_composition_areas (id,run_id,area_key,composition,path_scopes) VALUES ($1,$2,$3,$4,$5) RETURNING id,run_id,area_key,composition,path_scopes,frozen_at,status").bind(Uuid::now_v7().to_string()).bind(i.run_id).bind(i.area_key).bind(i.composition).bind(i.path_scopes).fetch_one(self.db.pool()).await.map_err(Into::into)
    }
    pub async fn create_attempt(
        &self,
        i: CreateReadinessAreaAttempt,
    ) -> Result<ReadinessAreaAttemptRow> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,$4,$5) RETURNING id,run_id,area_id,attempt_number,correlation_key,status,payload_digest,created_at,terminal_at").bind(Uuid::now_v7().to_string()).bind(i.run_id).bind(i.area_id).bind(i.attempt_number).bind(i.correlation_key).fetch_one(self.db.pool()).await.map_err(Into::into)
    }
    /// Inserts all accepted output and its event in one transaction.
    pub async fn accept_result(
        &self,
        a: &ReadinessAreaAttemptRow,
        findings: &[NewReadinessFinding],
        suggestions: &[NewReadinessSuggestion],
        event: NewReadinessEvent,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT run_id,area_id,status FROM readiness_area_attempts WHERE id=$1 FOR UPDATE",
        )
        .bind(&a.id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((run, area, status)) = row else {
            return Err(Error::InvalidData("readiness attempt not found".into()));
        };
        if run != a.run_id || area != a.area_id || status != "running" {
            return Err(Error::InvalidTransition(
                "readiness attempt is not the current running correlation".into(),
            ));
        };
        for f in findings {
            sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity,accepted,evidence) VALUES ($1,$2,$3,$4,$5,$6,true,$7)").bind(Uuid::now_v7().to_string()).bind(&run).bind(&area).bind(&a.id).bind(&f.guardrail_key).bind(&f.severity).bind(&f.evidence).execute(&mut *tx).await?;
        }
        for s in suggestions {
            sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ($1,$2,$3,$4) ON CONFLICT (run_id,dedupe_key) DO NOTHING").bind(Uuid::now_v7().to_string()).bind(&run).bind(&s.dedupe_key).bind(&s.suggestion).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE readiness_area_attempts SET status='succeeded',terminal_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$1").bind(&a.id).execute(&mut *tx).await?;
        sqlx::query("UPDATE readiness_composition_areas SET status='succeeded' WHERE id=$1")
            .bind(&area)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,$3,$4)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&run)
        .bind(event.event_kind)
        .bind(event.payload)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn active_or_latest_for_project(&self, p: &str) -> Result<Option<ReadinessRunRow>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at FROM readiness_runs WHERE project_id=$1 ORDER BY (status IN ('identifying','analyzing','aggregating')) DESC,created_at DESC LIMIT 1").bind(p).fetch_optional(self.db.pool()).await.map_err(Into::into)
    }
    pub async fn append_event(
        &self,
        run: &str,
        e: NewReadinessEvent,
    ) -> Result<ReadinessRunEventRow> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,$3,$4) RETURNING id,run_id,event_kind,payload,created_at").bind(Uuid::now_v7().to_string()).bind(run).bind(e.event_kind).bind(e.payload).fetch_one(self.db.pool()).await.map_err(Into::into)
    }
}
