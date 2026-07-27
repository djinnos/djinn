//! Typed, transactional persistence for Agent Readiness runs.
use crate::{
    Error, Result,
    database::Database,
    repositories::task::{
        ReadinessAreaAnalysisTask, ReadinessIdentificationTask,
        create_readiness_area_analysis_task_in_transaction,
        create_readiness_identification_task_in_transaction, load_task_in_transaction,
    },
};
use djinn_core::models::Task;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// Validate the complete success contract before any result state is visible.
fn validate_success(result: &serde_json::Value) -> Result<()> {
    let object = result
        .as_object()
        .ok_or_else(|| Error::InvalidData("readiness result must be an object".into()))?;
    for key in ["findings", "unsupported", "warnings", "remediation_suggestions"] {
        if !object.get(key).is_some_and(serde_json::Value::is_array) {
            return Err(Error::InvalidData(format!("readiness result {key} must be an array")));
        }
    }
    for finding in object["findings"].as_array().expect("array checked") {
        let Some(finding) = finding.as_object() else {
            return Err(Error::InvalidData("readiness finding must be an object".into()));
        };
        let evidence = finding.get("evidence");
        let evidence_is_structured = matches!(evidence, Some(serde_json::Value::Object(values)) if !values.is_empty())
            || matches!(evidence, Some(serde_json::Value::Array(values)) if !values.is_empty());
        if !finding.get("guardrail_key").and_then(serde_json::Value::as_str).is_some_and(|value| !value.trim().is_empty())
            || !matches!(finding.get("severity").and_then(serde_json::Value::as_str), Some("info" | "low" | "medium" | "high" | "critical"))
            || !evidence_is_structured
            || !finding.get("confidence").and_then(serde_json::Value::as_f64).is_some_and(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        {
            return Err(Error::InvalidData("invalid structured readiness finding".into()));
        }
    }
    for key in ["unsupported", "warnings"] {
        for entry in object[key].as_array().expect("array checked") {
            if !entry.as_object().is_some_and(|value| !value.is_empty()) {
                return Err(Error::InvalidData(format!("readiness {key} entry must be a non-empty object")));
            }
        }
    }
    for entry in object["remediation_suggestions"].as_array().expect("array checked") {
        let Some(suggestion) = entry.as_object() else {
            return Err(Error::InvalidData("readiness remediation suggestion must be an object".into()));
        };
        if !suggestion.get("dedupe_key").and_then(serde_json::Value::as_str).is_some_and(|value| !value.trim().is_empty())
            || suggestion.len() < 2
        {
            return Err(Error::InvalidData("invalid structured readiness remediation suggestion".into()));
        }
    }
    Ok(())
}

/// Render JSON with sorted object keys and sorted array representations. Readiness
/// output is set-like, so redelivery order cannot change its idempotency digest.
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            format!("{{{}}}", entries.into_iter().map(|(key, value)| format!("{}:{}", serde_json::to_string(key).expect("JSON key"), canonical(value))).collect::<Vec<_>>().join(","))
        }
        serde_json::Value::Array(values) => {
            let mut values = values.iter().map(canonical).collect::<Vec<_>>();
            values.sort();
            format!("[{}]", values.join(","))
        }
        _ => serde_json::to_string(value).expect("JSON value"),
    }
}

fn callback_digest(callback: &ReadinessAreaResultCallback) -> Result<String> {
    if [
        &callback.run_id, &callback.area_id, &callback.attempt_id, &callback.correlation_key, &callback.task_id,
    ].iter().any(|value| value.trim().is_empty()) {
        return Err(Error::InvalidData("readiness callback correlation fields must be non-empty".into()));
    }
    Ok(format!("sha256:{:x}", Sha256::digest(canonical(&serde_json::json!({
        "status": callback.status,
        "result": callback.result,
    })))))
}

#[derive(Clone, Debug)]
pub struct ReadinessAreaResultCallback {
    pub run_id: String,
    pub area_id: String,
    pub attempt_id: String,
    pub correlation_key: String,
    pub task_id: String,
    pub status: String,
    pub result: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadinessCallbackOutcome { Accepted, Redelivered, Ignored, Conflict }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessIdentificationOutput { pub areas: Vec<ReadinessIdentifiedArea> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessIdentifiedArea {
    pub area_key: String, pub path_scopes: Vec<String>, pub languages: Vec<String>, pub roles: Vec<String>,
    pub frameworks: Vec<String>, pub key_libraries: Vec<String>, pub confidence: f64, pub evidence: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct ReadinessAreaFanout { pub area: ReadinessCompositionAreaRow, pub attempt: ReadinessAreaAttemptRow, pub task: Task }
#[derive(Clone, Debug)]
pub struct RetryReadinessAreaAttempt { pub run_id: String, pub area_id: String, pub creator_user_id: String }

pub fn validate_identification_output(output: ReadinessIdentificationOutput) -> std::result::Result<Vec<ReadinessIdentifiedArea>, String> {
    if output.areas.is_empty() { return Err("identification output must contain at least one area".into()); }
    let mut keys = std::collections::HashSet::new();
    for area in &output.areas {
        let key = area.area_key.as_bytes();
        if key.is_empty() || !key[0].is_ascii_lowercase() || !key.iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-') || !keys.insert(&area.area_key) { return Err(format!("invalid or duplicate stable area_key: {}", area.area_key)); }
        if area.path_scopes.is_empty() || area.path_scopes.iter().any(|scope| !valid_repo_relative_scope(scope)) { return Err(format!("area {} has an invalid path scope", area.area_key)); }
        for (name, values) in [("languages", &area.languages), ("roles", &area.roles), ("frameworks", &area.frameworks), ("key_libraries", &area.key_libraries)] {
            if !normalized_unique(values) { return Err(format!("area {} has unnormalized or duplicate {name}", area.area_key)); }
        }
        if !area.confidence.is_finite() || !(0.0..=1.0).contains(&area.confidence) { return Err(format!("area {} has invalid confidence", area.area_key)); }
        if area.evidence.is_empty() || area.evidence.iter().any(|value| value.trim().is_empty()) { return Err(format!("area {} must include evidence", area.area_key)); }
    }
    Ok(output.areas)
}
fn normalized_unique(values: &[String]) -> bool { let mut seen = std::collections::HashSet::new(); values.iter().all(|value| !value.is_empty() && value.trim() == value && seen.insert(value)) }
fn valid_repo_relative_scope(scope: &str) -> bool { !scope.trim().is_empty() && scope.trim() == scope && !scope.starts_with('/') && !scope.starts_with('\\') && !scope.split(['/', '\\']).any(|part| part == "..") }
#[derive(Clone, Debug)]
pub struct MaterializeReadinessKickoff {
    pub project_id: String,
    pub creator_user_id: String,
    pub idempotency_key: String,
    pub repository_snapshot: String,
    pub skill_name: String,
    pub skill_version: String,
}
#[derive(Clone, Debug)]
pub struct ReadinessKickoffMaterialization {
    pub run: ReadinessRunRow,
    pub identification_task: Task,
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
    pub async fn materialize_kickoff(
        &self,
        input: MaterializeReadinessKickoff,
    ) -> Result<ReadinessKickoffMaterialization> {
        for (field, value) in [
            ("project_id", &input.project_id),
            ("creator_user_id", &input.creator_user_id),
            ("idempotency_key", &input.idempotency_key),
            ("repository_snapshot", &input.repository_snapshot),
            ("skill_name", &input.skill_name),
            ("skill_version", &input.skill_version),
        ] {
            if value.trim().is_empty() {
                return Err(Error::InvalidData(format!(
                    "readiness kickoff {field} must be non-empty"
                )));
            }
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&input.project_id)
            .execute(&mut *tx)
            .await?;
        let active: Option<ReadinessRunRow> = sqlx::query_as("SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at FROM readiness_runs WHERE project_id=$1 AND status IN ('identifying','analyzing','aggregating') FOR UPDATE").bind(&input.project_id).fetch_optional(&mut *tx).await?;
        let run = match active {
            Some(run) => run,
            None => sqlx::query_as("INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at")
                .bind(Uuid::now_v7().to_string()).bind(&input.project_id).bind(&input.idempotency_key).bind(&input.repository_snapshot).bind(&input.skill_name).bind(&input.skill_version).fetch_one(&mut *tx).await?,
        };
        let task_id: Option<String> = sqlx::query_scalar("SELECT id FROM tasks WHERE project_id=$1 AND (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END) ->> 'kind' = 'readiness_identification' AND (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END) ->> 'run_id' = $2 FOR UPDATE").bind(&run.project_id).bind(&run.id).fetch_optional(&mut *tx).await?;
        let identification_task = match task_id {
            Some(id) => load_task_in_transaction(&mut tx, &id).await?,
            None => {
                create_readiness_identification_task_in_transaction(
                    &mut tx,
                    ReadinessIdentificationTask {
                        project_id: &run.project_id,
                        creator_user_id: &input.creator_user_id,
                        run_id: &run.id,
                        repository_snapshot: &run.repository_snapshot,
                        skill_name: &run.skill_name,
                        skill_version: &run.skill_version,
                    },
                )
                .await?
            }
        };
        tx.commit().await?;
        Ok(ReadinessKickoffMaterialization {
            run,
            identification_task,
        })
    }
    pub async fn complete_identification(
        &self,
        run_id: &str,
        creator_user_id: &str,
        output: ReadinessIdentificationOutput,
    ) -> Result<Vec<ReadinessAreaFanout>> {
        self.db.ensure_initialized().await?;
        let validated = validate_identification_output(output);
        let mut tx = self.db.pool().begin().await?;
        let run: ReadinessRunRow = sqlx::query_as("SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at FROM readiness_runs WHERE id=$1 FOR UPDATE").bind(run_id).fetch_optional(&mut *tx).await?.ok_or_else(|| Error::InvalidData("readiness run not found".into()))?;
        if run.status != "identifying" {
            return Err(Error::InvalidTransition(
                "readiness run is not identifying".into(),
            ));
        }
        let areas = match validated {
            Ok(areas) => areas,
            Err(reason) => {
                Self::fail_identification_tx(&mut tx, &run.id, &reason).await?;
                tx.commit().await?;
                return Err(Error::InvalidData(reason));
            }
        };
        let count = i32::try_from(areas.len())
            .map_err(|_| Error::InvalidData("too many readiness areas".into()))?;
        let mut fanout = Vec::with_capacity(areas.len());
        for identified in areas {
            let composition = serde_json::json!({"languages":identified.languages,"roles":identified.roles,"frameworks":identified.frameworks,"key_libraries":identified.key_libraries,"confidence":identified.confidence,"evidence":identified.evidence});
            let scopes = serde_json::to_value(&identified.path_scopes)
                .map_err(|error| Error::InvalidData(error.to_string()))?;
            let area: ReadinessCompositionAreaRow = sqlx::query_as("INSERT INTO readiness_composition_areas (id,run_id,area_key,composition,path_scopes) VALUES ($1,$2,$3,$4,$5) RETURNING id,run_id,area_key,composition,path_scopes,frozen_at,status").bind(Uuid::now_v7().to_string()).bind(&run.id).bind(&identified.area_key).bind(&composition).bind(&scopes).fetch_one(&mut *tx).await?;
            let attempt: ReadinessAreaAttemptRow = sqlx::query_as("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,1,$4) RETURNING id,run_id,area_id,attempt_number,correlation_key,status,payload_digest,created_at,terminal_at").bind(Uuid::now_v7().to_string()).bind(&run.id).bind(&area.id).bind(Uuid::now_v7().to_string()).fetch_one(&mut *tx).await?;
            let task = create_readiness_area_analysis_task_in_transaction(
                &mut tx,
                ReadinessAreaAnalysisTask {
                    project_id: &run.project_id,
                    creator_user_id,
                    run_id: &run.id,
                    area_id: &area.id,
                    area_key: &area.area_key,
                    attempt_id: &attempt.id,
                    attempt_number: 1,
                    correlation_key: &attempt.correlation_key,
                    repository_snapshot: &run.repository_snapshot,
                    skill_name: &run.skill_name,
                    skill_version: &run.skill_version,
                    composition: &area.composition,
                    path_scopes: &area.path_scopes,
                },
            )
            .await?;
            fanout.push(ReadinessAreaFanout {
                area,
                attempt,
                task,
            });
        }
        sqlx::query(
            "UPDATE readiness_runs SET status='analyzing',expected_area_count=$1 WHERE id=$2",
        )
        .bind(count)
        .bind(&run.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,'identification_completed',$3)").bind(Uuid::now_v7().to_string()).bind(&run.id).bind(serde_json::json!({"expected_area_count":count})).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(fanout)
    }
    pub async fn fail_identification(&self, run_id: &str, reason: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM readiness_runs WHERE id=$1 FOR UPDATE")
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await?;
        if status.as_deref() != Some("identifying") {
            return Err(Error::InvalidTransition(
                "readiness run is not identifying".into(),
            ));
        }
        Self::fail_identification_tx(&mut tx, run_id, reason).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn fail_identification_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        reason: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,'identification_failed',$3)").bind(Uuid::now_v7().to_string()).bind(run_id).bind(serde_json::json!({"reason":reason})).execute(&mut **tx).await?;
        sqlx::query("UPDATE readiness_runs SET status='failed',completed_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$1").bind(run_id).execute(&mut **tx).await?;
        Ok(())
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
    /// First terminal callback wins. The attempt and current-area rows are locked
    /// before the whole accepted set becomes visible.
    pub async fn ingest_area_result(&self, callback: ReadinessAreaResultCallback) -> Result<ReadinessCallbackOutcome> {
        self.db.ensure_initialized().await?;
        let digest = callback_digest(&callback)?;
        let mut tx = self.db.pool().begin().await?;
        let row: Option<(String,String,String,String,Option<String>)> = sqlx::query_as("SELECT run_id,area_id,correlation_key,status,payload_digest FROM readiness_area_attempts WHERE id=$1 FOR UPDATE").bind(&callback.attempt_id).fetch_optional(&mut *tx).await?;
        let Some((run, area, key, status, old)) = row else { return Err(Error::InvalidData("readiness callback attempt not found".into())); };
        let record = |kind: &str, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>| async { sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,$3,$4)").bind(Uuid::now_v7().to_string()).bind(&run).bind(kind).bind(serde_json::json!({"attempt_id":callback.attempt_id,"area_id":callback.area_id,"correlation_key":callback.correlation_key,"task_id":callback.task_id,"digest":digest})).execute(&mut **tx).await };
        let task_matches: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tasks WHERE id=$1 AND (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END) @> jsonb_build_object('kind','readiness_area_analysis','run_id',$2,'area_id',$3,'attempt_id',$4,'correlation_key',$5))").bind(&callback.task_id).bind(&run).bind(&area).bind(&callback.attempt_id).bind(&key).fetch_one(&mut *tx).await?;
        if run != callback.run_id || area != callback.area_id || key != callback.correlation_key || !task_matches { record("readiness_callback_ignored", &mut tx).await?; tx.commit().await?; return Ok(ReadinessCallbackOutcome::Ignored); }
        let current: Option<String> = sqlx::query_scalar("SELECT id FROM readiness_area_attempts WHERE area_id=$1 ORDER BY attempt_number DESC LIMIT 1 FOR UPDATE").bind(&area).fetch_optional(&mut *tx).await?;
        if current.as_deref() != Some(callback.attempt_id.as_str()) { record("readiness_callback_ignored", &mut tx).await?; tx.commit().await?; return Ok(ReadinessCallbackOutcome::Ignored); }
        if status != "running" { let outcome = if old.as_deref() == Some(&digest) { ReadinessCallbackOutcome::Redelivered } else { record("readiness_callback_conflict", &mut tx).await?; ReadinessCallbackOutcome::Conflict }; tx.commit().await?; return Ok(outcome); }
        let valid = callback.status == "succeeded" && validate_success(&callback.result).is_ok();
        let final_status = if valid { "succeeded" } else { match callback.status.as_str() { "failed" | "timed_out" | "invalid" => callback.status.as_str(), _ => "invalid" } };
        if valid { for finding in callback.result["findings"].as_array().expect("validated") { sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity,accepted,evidence) VALUES ($1,$2,$3,$4,$5,$6,true,$7)").bind(Uuid::now_v7().to_string()).bind(&run).bind(&area).bind(&callback.attempt_id).bind(finding["guardrail_key"].as_str().expect("validated")).bind(finding["severity"].as_str().expect("validated")).bind(finding["evidence"].clone()).execute(&mut *tx).await?; } for suggestion in callback.result["remediation_suggestions"].as_array().expect("validated") { sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ($1,$2,$3,$4) ON CONFLICT (run_id,dedupe_key) DO NOTHING").bind(Uuid::now_v7().to_string()).bind(&run).bind(suggestion["dedupe_key"].as_str().expect("validated")).bind(suggestion.clone()).execute(&mut *tx).await?; } }
        sqlx::query("UPDATE readiness_area_attempts SET status=$1,payload_digest=$2,terminal_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$3").bind(final_status).bind(&digest).bind(&callback.attempt_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE readiness_composition_areas SET status=$1 WHERE id=$2").bind(final_status).bind(&area).execute(&mut *tx).await?;
        record(if valid { "readiness_result_accepted" } else { "readiness_result_terminal_failure" }, &mut tx).await?; tx.commit().await?; Ok(ReadinessCallbackOutcome::Accepted)
    }

    pub async fn retry_area_attempt(&self, input: RetryReadinessAreaAttempt) -> Result<ReadinessAreaFanout> {
        self.db.ensure_initialized().await?; let mut tx = self.db.pool().begin().await?;
        let run: ReadinessRunRow = sqlx::query_as("SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at FROM readiness_runs WHERE id=$1 FOR UPDATE").bind(&input.run_id).fetch_optional(&mut *tx).await?.ok_or_else(|| Error::InvalidData("readiness run not found".into()))?;
        if !matches!(run.status.as_str(), "identifying" | "analyzing" | "aggregating") { return Err(Error::InvalidTransition("cannot retry an area in a terminal readiness run".into())); }
        let area: ReadinessCompositionAreaRow = sqlx::query_as("SELECT id,run_id,area_key,composition,path_scopes,frozen_at,status FROM readiness_composition_areas WHERE id=$1 AND run_id=$2 FOR UPDATE").bind(&input.area_id).bind(&run.id).fetch_optional(&mut *tx).await?.ok_or_else(|| Error::InvalidData("readiness area not found".into()))?;
        let previous: ReadinessAreaAttemptRow = sqlx::query_as("SELECT id,run_id,area_id,attempt_number,correlation_key,status,payload_digest,created_at,terminal_at FROM readiness_area_attempts WHERE area_id=$1 ORDER BY attempt_number DESC LIMIT 1 FOR UPDATE").bind(&area.id).fetch_optional(&mut *tx).await?.ok_or_else(|| Error::InvalidData("readiness area has no attempt".into()))?;
        if !matches!(previous.status.as_str(), "failed" | "timed_out" | "invalid") { return Err(Error::InvalidTransition("only the current failed, timed-out, or invalid attempt may be retried".into())); }
        let attempt: ReadinessAreaAttemptRow = sqlx::query_as("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,$4,$5) RETURNING id,run_id,area_id,attempt_number,correlation_key,status,payload_digest,created_at,terminal_at").bind(Uuid::now_v7().to_string()).bind(&run.id).bind(&area.id).bind(previous.attempt_number + 1).bind(Uuid::now_v7().to_string()).fetch_one(&mut *tx).await?;
        let task = create_readiness_area_analysis_task_in_transaction(&mut tx, ReadinessAreaAnalysisTask { project_id: &run.project_id, creator_user_id: &input.creator_user_id, run_id: &run.id, area_id: &area.id, area_key: &area.area_key, attempt_id: &attempt.id, attempt_number: attempt.attempt_number, correlation_key: &attempt.correlation_key, repository_snapshot: &run.repository_snapshot, skill_name: &run.skill_name, skill_version: &run.skill_version, composition: &area.composition, path_scopes: &area.path_scopes }).await?;
        sqlx::query("UPDATE readiness_composition_areas SET status='running' WHERE id=$1").bind(&area.id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,'readiness_attempt_retried',$3)").bind(Uuid::now_v7().to_string()).bind(&run.id).bind(serde_json::json!({"area_id":area.id,"prior_attempt_id":previous.id,"attempt_id":attempt.id,"task_id":task.id})).execute(&mut *tx).await?;
        tx.commit().await?; Ok(ReadinessAreaFanout { area, attempt, task })
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
