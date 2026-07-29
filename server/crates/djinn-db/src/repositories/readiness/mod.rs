//! Typed, transactional persistence for Agent Readiness runs.
// djinn:allow-oversize -- readiness transaction invariants remain co-located.
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

fn reference_values(value: Option<&serde_json::Value>, key: &str) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get(key))
        .map(|value| match value {
            serde_json::Value::String(value) => vec![value.clone()],
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

/// Canonicalize set-like suggestion references and make conflicting descriptive
/// values independent of which area's callback arrived first.
fn canonical_suggestion(
    suggestion: &serde_json::Value,
    area_id: &str,
    prior: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut result = suggestion
        .as_object()
        .cloned()
        .expect("validated suggestion");
    let mut area_ids = reference_values(prior, "area_ids");
    area_ids.extend(reference_values(Some(suggestion), "area_ids"));
    area_ids.extend(reference_values(prior, "area_id"));
    area_ids.extend(reference_values(Some(suggestion), "area_id"));
    area_ids.push(area_id.to_owned());
    let mut guardrail_ids = reference_values(prior, "guardrail_ids");
    guardrail_ids.extend(reference_values(Some(suggestion), "guardrail_ids"));
    guardrail_ids.extend(reference_values(prior, "guardrail_id"));
    guardrail_ids.extend(reference_values(Some(suggestion), "guardrail_id"));
    area_ids.sort();
    area_ids.dedup();
    guardrail_ids.sort();
    guardrail_ids.dedup();
    result.insert("area_ids".into(), serde_json::json!(area_ids));
    result.insert("guardrail_ids".into(), serde_json::json!(guardrail_ids));
    // Singular references are folded into their canonical sorted sets above.
    // Keeping either one would make the stored value depend on callback order.
    result.remove("area_id");
    result.remove("guardrail_id");
    if let Some(prior) = prior.and_then(serde_json::Value::as_object) {
        for (key, prior_value) in prior {
            if matches!(
                key.as_str(),
                "area_id" | "area_ids" | "guardrail_id" | "guardrail_ids"
            ) {
                continue;
            }
            if let Some(value) = result.get(key) {
                if canonical(prior_value) < canonical(value) {
                    result.insert(key.clone(), prior_value.clone());
                }
            } else {
                result.insert(key.clone(), prior_value.clone());
            }
        }
    }
    serde_json::Value::Object(result)
}

async fn merge_suggestion(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    area_id: &str,
    dedupe_key: &str,
    suggestion: &serde_json::Value,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{run_id}:{dedupe_key}"))
        .execute(&mut **tx)
        .await?;
    let prior: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT suggestion FROM readiness_remediation_suggestions WHERE run_id=$1 AND dedupe_key=$2 FOR UPDATE",
    )
    .bind(run_id)
    .bind(dedupe_key)
    .fetch_optional(&mut **tx)
    .await?;
    let merged = canonical_suggestion(suggestion, area_id, prior.as_ref());
    if prior.is_some() {
        sqlx::query("UPDATE readiness_remediation_suggestions SET suggestion=$1 WHERE run_id=$2 AND dedupe_key=$3")
            .bind(merged).bind(run_id).bind(dedupe_key).execute(&mut **tx).await?;
    } else {
        sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ($1,$2,$3,$4)")
            .bind(Uuid::now_v7().to_string()).bind(run_id).bind(dedupe_key).bind(merged).execute(&mut **tx).await?;
    }
    Ok(())
}

/// Validate the complete success contract before any result state is visible.
fn validate_success(result: &serde_json::Value) -> Result<()> {
    let object = result
        .as_object()
        .ok_or_else(|| Error::InvalidData("readiness result must be an object".into()))?;
    for key in [
        "findings",
        "unsupported",
        "warnings",
        "remediation_suggestions",
    ] {
        if !object.get(key).is_some_and(serde_json::Value::is_array) {
            return Err(Error::InvalidData(format!(
                "readiness result {key} must be an array"
            )));
        }
    }
    let mut finding_keys = std::collections::HashSet::new();
    for finding in object["findings"].as_array().expect("array checked") {
        let Some(finding) = finding.as_object() else {
            return Err(Error::InvalidData(
                "readiness finding must be an object".into(),
            ));
        };
        let evidence_is_structured = finding.get("evidence").is_some_and(structured_evidence);
        let guardrail_key = finding
            .get("guardrail_key")
            .and_then(serde_json::Value::as_str);
        if guardrail_key.is_none_or(|value| value.trim().is_empty())
            || !finding_keys.insert(guardrail_key.expect("checked non-empty"))
            || !matches!(
                finding.get("status").and_then(serde_json::Value::as_str),
                Some(
                    "covered"
                        | "partial"
                        | "missing"
                        | "unknown"
                        | "unsupported"
                        | "analysis_error"
                )
            )
            || !matches!(
                finding.get("severity").and_then(serde_json::Value::as_str),
                Some("low" | "medium" | "high" | "critical")
            )
            || !evidence_is_structured
            || !finding
                .get("confidence")
                .and_then(serde_json::Value::as_f64)
                .is_some_and(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        {
            return Err(Error::InvalidData(
                "invalid structured readiness finding".into(),
            ));
        }
    }
    for key in ["unsupported", "warnings"] {
        for entry in object[key].as_array().expect("array checked") {
            if !structured_object(entry) {
                return Err(Error::InvalidData(format!(
                    "readiness {key} entry must be a non-empty object"
                )));
            }
        }
    }
    let mut suggestion_keys = std::collections::HashSet::new();
    for entry in object["remediation_suggestions"]
        .as_array()
        .expect("array checked")
    {
        let Some(suggestion) = entry.as_object() else {
            return Err(Error::InvalidData(
                "readiness remediation suggestion must be an object".into(),
            ));
        };
        let dedupe_key = suggestion
            .get("dedupe_key")
            .and_then(serde_json::Value::as_str);
        if dedupe_key.is_none_or(|value| value.trim().is_empty())
            || !suggestion_keys.insert(dedupe_key.expect("checked non-empty"))
            || suggestion.len() < 2
            || !structured_object(entry)
        {
            return Err(Error::InvalidData(
                "invalid structured readiness remediation suggestion".into(),
            ));
        }
    }
    Ok(())
}

fn structured_object(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|object| {
        !object.is_empty()
            && object
                .iter()
                .all(|(key, value)| !key.trim().is_empty() && !value.is_null())
    })
}

fn structured_evidence(value: &serde_json::Value) -> bool {
    structured_object(value)
        || value
            .as_array()
            .is_some_and(|entries| !entries.is_empty() && entries.iter().all(structured_object))
}

/// Render JSON with sorted object keys and sorted array representations. Readiness
/// output is set-like, so redelivery order cannot change its idempotency digest.
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(left, _)| *left);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON key"),
                        canonical(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
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
        &callback.run_id,
        &callback.area_id,
        &callback.attempt_id,
        &callback.correlation_key,
        &callback.task_id,
    ]
    .iter()
    .any(|value| value.trim().is_empty())
    {
        return Err(Error::InvalidData(
            "readiness callback correlation fields must be non-empty".into(),
        ));
    }
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(canonical(&serde_json::json!({
            "status": callback.status,
            "result": callback.result,
        })))
    ))
}

async fn record_callback_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    callback: &ReadinessAreaResultCallback,
    digest: &str,
    event_kind: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,$3,$4)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(run_id)
    .bind(event_kind)
    .bind(serde_json::json!({
        "attempt_id": callback.attempt_id,
        "area_id": callback.area_id,
        "correlation_key": callback.correlation_key,
        "task_id": callback.task_id,
        "digest": digest,
    }))
    .execute(&mut **tx)
    .await?;
    Ok(())
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
pub enum ReadinessCallbackOutcome {
    Accepted,
    Redelivered,
    Ignored,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessIdentificationOutput {
    pub areas: Vec<ReadinessIdentifiedArea>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadinessIdentifiedArea {
    pub area_key: String,
    pub path_scopes: Vec<String>,
    pub languages: Vec<String>,
    pub roles: Vec<String>,
    pub frameworks: Vec<String>,
    pub key_libraries: Vec<String>,
    pub confidence: f64,
    pub evidence: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct ReadinessAreaFanout {
    pub area: ReadinessCompositionAreaRow,
    pub attempt: ReadinessAreaAttemptRow,
    pub task: Task,
}
#[derive(Clone, Debug)]
pub struct RetryReadinessAreaAttempt {
    pub run_id: String,
    pub area_id: String,
    pub creator_user_id: String,
}

pub fn validate_identification_output(
    output: ReadinessIdentificationOutput,
) -> std::result::Result<Vec<ReadinessIdentifiedArea>, String> {
    if output.areas.is_empty() {
        return Err("identification output must contain at least one area".into());
    }
    let mut keys = std::collections::HashSet::new();
    for area in &output.areas {
        let key = area.area_key.as_bytes();
        if key.is_empty()
            || !key[0].is_ascii_lowercase()
            || !key.iter().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
            })
            || !keys.insert(&area.area_key)
        {
            return Err(format!(
                "invalid or duplicate stable area_key: {}",
                area.area_key
            ));
        }
        if area.path_scopes.is_empty()
            || area
                .path_scopes
                .iter()
                .any(|scope| !valid_repo_relative_scope(scope))
        {
            return Err(format!("area {} has an invalid path scope", area.area_key));
        }
        for (name, values) in [
            ("languages", &area.languages),
            ("roles", &area.roles),
            ("frameworks", &area.frameworks),
            ("key_libraries", &area.key_libraries),
        ] {
            if !normalized_unique(values) {
                return Err(format!(
                    "area {} has unnormalized or duplicate {name}",
                    area.area_key
                ));
            }
        }
        if !area.confidence.is_finite() || !(0.0..=1.0).contains(&area.confidence) {
            return Err(format!("area {} has invalid confidence", area.area_key));
        }
        if area.evidence.is_empty() || area.evidence.iter().any(|value| value.trim().is_empty()) {
            return Err(format!("area {} must include evidence", area.area_key));
        }
    }
    Ok(output.areas)
}
fn normalized_unique(values: &[String]) -> bool {
    let mut seen = std::collections::HashSet::new();
    values
        .iter()
        .all(|value| !value.is_empty() && value.trim() == value && seen.insert(value))
}
fn valid_repo_relative_scope(scope: &str) -> bool {
    !scope.trim().is_empty()
        && scope.trim() == scope
        && !scope.starts_with('/')
        && !scope.starts_with('\\')
        && !scope.split(['/', '\\']).any(|part| part == "..")
}
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
    pub status: String,
    pub severity: String,
    pub confidence: f64,
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
pub struct ReadinessAreaScoreRow {
    pub run_id: String,
    pub area_id: String,
    pub score: f64,
    pub applicable_weight: i32,
    pub covered_weight: f64,
    pub status: String,
    pub created_at: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ReadinessProjectScoreRow {
    pub run_id: String,
    pub score: f64,
    pub band: String,
    pub created_at: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ReadinessAggregation {
    pub area_scores: Vec<ReadinessAreaScoreRow>,
    pub project_score: ReadinessProjectScoreRow,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessGuardrailStatus {
    Covered,
    Partial,
    Missing,
    Unknown,
    Unsupported,
    AnalysisError,
}
impl ReadinessGuardrailStatus {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "covered" => Self::Covered,
            "partial" => Self::Partial,
            "missing" => Self::Missing,
            "unknown" => Self::Unknown,
            "unsupported" => Self::Unsupported,
            "analysis_error" => Self::AnalysisError,
            _ => return None,
        })
    }
}
pub fn readiness_severity_weight(severity: &str) -> Option<i32> {
    Some(match severity {
        "critical" => 5,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => return None,
    })
}
/// Exact proposal arithmetic. Covered evidence below 0.7 is capped at partial.
pub fn readiness_area_score(findings: &[(String, String, f64)]) -> (f64, i32, f64, String) {
    let mut applicable = 0;
    let mut covered = 0.0;
    for (status, severity, confidence) in findings {
        let Some(status) = ReadinessGuardrailStatus::parse(status) else {
            continue;
        };
        let Some(weight) = readiness_severity_weight(severity) else {
            continue;
        };
        if status == ReadinessGuardrailStatus::Unsupported {
            continue;
        }
        applicable += weight;
        covered += f64::from(weight)
            * match status {
                ReadinessGuardrailStatus::Covered if *confidence >= 0.7 => 1.0,
                ReadinessGuardrailStatus::Covered | ReadinessGuardrailStatus::Partial => 0.5,
                ReadinessGuardrailStatus::Missing
                | ReadinessGuardrailStatus::Unknown
                | ReadinessGuardrailStatus::AnalysisError
                | ReadinessGuardrailStatus::Unsupported => 0.0,
            };
    }
    let score = if applicable == 0 {
        0.0
    } else {
        covered / f64::from(applicable)
    };
    (
        score,
        applicable,
        covered,
        if applicable == 0 {
            "unsupported"
        } else {
            "supported"
        }
        .into(),
    )
}
/// Applicable-weighted mean; zero-applicable (unsupported) areas contribute
/// neither numerator nor denominator.
pub fn readiness_project_score(area_scores: &[(i32, f64)]) -> f64 {
    let (applicable, covered) = area_scores
        .iter()
        .fold((0, 0.0), |(weight, total), (area_weight, area_covered)| {
            (weight + area_weight, total + area_covered)
        });
    if applicable == 0 {
        0.0
    } else {
        covered / f64::from(applicable)
    }
}
pub fn readiness_score_band(score: f64) -> &'static str {
    if score < 0.40 {
        "blocked"
    } else if score < 0.70 {
        "emerging"
    } else if score < 0.85 {
        "ready"
    } else {
        "strong"
    }
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
    pub status: String,
    pub severity: String,
    pub confidence: f64,
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
    pub areas: Vec<ReadinessRunDetailArea>,
    /// Persisted aggregation output, ordered by frozen area identity.
    pub area_scores: Vec<ReadinessAreaScoreRow>,
    /// Absent until aggregation persists the run's single project score.
    pub project_score: Option<ReadinessProjectScoreRow>,
    /// One canonical persisted suggestion per run/dedupe key.
    pub suggestions: Vec<ReadinessRemediationSuggestionRow>,
    /// Append-only lifecycle history ordered by creation time and identity.
    pub events: Vec<ReadinessRunEventRow>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ReadinessRunDetailArea {
    pub area: ReadinessCompositionAreaRow,
    pub attempts: Vec<ReadinessRunDetailAttempt>,
    pub accepted_findings: Vec<ReadinessGuardrailFindingRow>,
    pub accepted_outputs: Vec<ReadinessAreaResultOutputRow>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ReadinessRunDetailAttempt {
    pub attempt: ReadinessAreaAttemptRow,
    pub is_current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ReadinessAreaResultOutputRow {
    pub run_id: String,
    pub area_id: String,
    pub attempt_id: String,
    pub result: serde_json::Value,
    pub created_at: String,
}
/// Internal row shape for detail reads, which additionally carries the
/// persisted identity used to mark the current attempt.
#[derive(FromRow)]
struct ReadinessCompositionAreaDetailRow {
    id: String,
    run_id: String,
    area_key: String,
    composition: serde_json::Value,
    path_scopes: serde_json::Value,
    frozen_at: String,
    status: String,
    current_attempt_id: Option<String>,
}

impl From<ReadinessCompositionAreaDetailRow> for ReadinessCompositionAreaRow {
    fn from(row: ReadinessCompositionAreaDetailRow) -> Self {
        Self {
            id: row.id,
            run_id: row.run_id,
            area_key: row.area_key,
            composition: row.composition,
            path_scopes: row.path_scopes,
            frozen_at: row.frozen_at,
            status: row.status,
        }
    }
}

pub struct ReadinessRepository {
    db: Database,
}
impl ReadinessRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
    /// Reads frozen composition, full attempt history, current accepted output, and
    /// persisted lifecycle and aggregation projections in explicit stable order.
    pub async fn run_detail(&self, run_id: &str) -> Result<Option<ReadinessRunDetail>> {
        self.db.ensure_initialized().await?;
        let Some(run) = sqlx::query_as("SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at FROM readiness_runs WHERE id=$1").bind(run_id).fetch_optional(self.db.pool()).await? else { return Ok(None); };
        let areas: Vec<ReadinessCompositionAreaDetailRow> = sqlx::query_as("SELECT id,run_id,area_key,composition,path_scopes,frozen_at,status,current_attempt_id FROM readiness_composition_areas WHERE run_id=$1 ORDER BY area_key,id").bind(run_id).fetch_all(self.db.pool()).await?;
        let attempts: Vec<ReadinessAreaAttemptRow> = sqlx::query_as("SELECT a.id,a.run_id,a.area_id,a.attempt_number,a.correlation_key,a.status,a.payload_digest,a.created_at,a.terminal_at FROM readiness_area_attempts a JOIN readiness_composition_areas area ON area.id=a.area_id WHERE a.run_id=$1 ORDER BY area.area_key,area.id,a.attempt_number,a.id").bind(run_id).fetch_all(self.db.pool()).await?;
        let findings: Vec<ReadinessGuardrailFindingRow> = sqlx::query_as("SELECT f.id,f.run_id,f.area_id,f.attempt_id,f.guardrail_key,f.status,f.severity,f.confidence,f.accepted,f.evidence,f.created_at FROM readiness_guardrail_findings f JOIN readiness_composition_areas area ON area.id=f.area_id AND area.current_attempt_id=f.attempt_id WHERE f.run_id=$1 AND f.accepted=true ORDER BY area.area_key,area.id,f.guardrail_key,f.id").bind(run_id).fetch_all(self.db.pool()).await?;
        let outputs: Vec<ReadinessAreaResultOutputRow> = sqlx::query_as("SELECT output.run_id,output.area_id,output.attempt_id,output.result,output.created_at FROM readiness_area_result_outputs output JOIN readiness_composition_areas area ON area.id=output.area_id AND area.current_attempt_id=output.attempt_id WHERE output.run_id=$1 ORDER BY area.area_key,area.id,output.attempt_id").bind(run_id).fetch_all(self.db.pool()).await?;
        let area_scores: Vec<ReadinessAreaScoreRow> = sqlx::query_as("SELECT run_id,area_id,score,applicable_weight,covered_weight,status,created_at FROM readiness_area_scores WHERE run_id=$1 ORDER BY area_id").bind(run_id).fetch_all(self.db.pool()).await?;
        let project_score: Option<ReadinessProjectScoreRow> = sqlx::query_as(
            "SELECT run_id,score,band,created_at FROM readiness_project_scores WHERE run_id=$1",
        )
        .bind(run_id)
        .fetch_optional(self.db.pool())
        .await?;
        let suggestions: Vec<ReadinessRemediationSuggestionRow> = sqlx::query_as("SELECT id,run_id,dedupe_key,suggestion,created_at FROM readiness_remediation_suggestions WHERE run_id=$1 ORDER BY dedupe_key,id").bind(run_id).fetch_all(self.db.pool()).await?;
        let events: Vec<ReadinessRunEventRow> = sqlx::query_as("SELECT id,run_id,event_kind,payload,created_at FROM readiness_run_events WHERE run_id=$1 ORDER BY created_at,id").bind(run_id).fetch_all(self.db.pool()).await?;
        let mut attempts_by_area: std::collections::HashMap<String, Vec<_>> =
            std::collections::HashMap::new();
        for attempt in attempts {
            attempts_by_area
                .entry(attempt.area_id.clone())
                .or_default()
                .push(attempt);
        }
        let mut findings_by_area: std::collections::HashMap<String, Vec<_>> =
            std::collections::HashMap::new();
        for finding in findings {
            findings_by_area
                .entry(finding.area_id.clone())
                .or_default()
                .push(finding);
        }
        let mut outputs_by_area: std::collections::HashMap<String, Vec<_>> =
            std::collections::HashMap::new();
        for output in outputs {
            outputs_by_area
                .entry(output.area_id.clone())
                .or_default()
                .push(output);
        }
        let mut detail_areas = Vec::with_capacity(areas.len());
        for area_detail in areas {
            let current_attempt_id = area_detail.current_attempt_id.clone();
            let area = ReadinessCompositionAreaRow::from(area_detail);
            let attempts = attempts_by_area
                .remove(&area.id)
                .unwrap_or_default()
                .into_iter()
                .map(|attempt| ReadinessRunDetailAttempt {
                    is_current: current_attempt_id.as_deref() == Some(attempt.id.as_str()),
                    attempt,
                })
                .collect::<Vec<_>>();
            if attempts.iter().filter(|attempt| attempt.is_current).count() != 1 {
                return Err(Error::InvalidData(format!(
                    "readiness area {} has an invalid current attempt relation",
                    area.id
                )));
            }
            detail_areas.push(ReadinessRunDetailArea {
                accepted_findings: findings_by_area.remove(&area.id).unwrap_or_default(),
                accepted_outputs: outputs_by_area.remove(&area.id).unwrap_or_default(),
                area,
                attempts,
            });
        }
        Ok(Some(ReadinessRunDetail {
            run,
            areas: detail_areas,
            area_scores,
            project_score,
            suggestions,
            events,
        }))
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
            sqlx::query("UPDATE readiness_composition_areas SET current_attempt_id=$1 WHERE id=$2")
                .bind(&attempt.id)
                .bind(&area.id)
                .execute(&mut *tx)
                .await?;
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
        let mut tx = self.db.pool().begin().await?;
        let area_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM readiness_composition_areas WHERE id=$1 AND run_id=$2 FOR UPDATE",
        )
        .bind(&i.area_id)
        .bind(&i.run_id)
        .fetch_optional(&mut *tx)
        .await?;
        if area_id.is_none() {
            return Err(Error::InvalidData(
                "readiness area not found for attempt run".into(),
            ));
        }
        let attempt: ReadinessAreaAttemptRow = sqlx::query_as("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,$4,$5) RETURNING id,run_id,area_id,attempt_number,correlation_key,status,payload_digest,created_at,terminal_at")
            .bind(Uuid::now_v7().to_string())
            .bind(&i.run_id)
            .bind(&i.area_id)
            .bind(i.attempt_number)
            .bind(i.correlation_key)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("UPDATE readiness_composition_areas SET current_attempt_id=$1 WHERE id=$2")
            .bind(&attempt.id)
            .bind(&i.area_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(attempt)
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
            sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,status,severity,confidence,accepted,evidence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,true,$9)").bind(Uuid::now_v7().to_string()).bind(&run).bind(&area).bind(&a.id).bind(&f.guardrail_key).bind(&f.status).bind(&f.severity).bind(f.confidence).bind(&f.evidence).execute(&mut *tx).await?;
        }
        for s in suggestions {
            merge_suggestion(&mut tx, &run, &area, &s.dedupe_key, &s.suggestion).await?;
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
    pub async fn ingest_area_result(
        &self,
        callback: ReadinessAreaResultCallback,
    ) -> Result<ReadinessCallbackOutcome> {
        self.db.ensure_initialized().await?;
        let digest = callback_digest(&callback)?;
        let mut tx = self.db.pool().begin().await?;
        let row: Option<(String,String,String,String,Option<String>)> = sqlx::query_as("SELECT run_id,area_id,correlation_key,status,payload_digest FROM readiness_area_attempts WHERE id=$1 FOR UPDATE").bind(&callback.attempt_id).fetch_optional(&mut *tx).await?;
        let Some((run, area, key, status, old)) = row else {
            return Err(Error::InvalidData(
                "readiness callback attempt not found".into(),
            ));
        };
        let task_matches: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tasks WHERE id=$1 AND (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END) @> jsonb_build_object('kind','readiness_area_analysis','run_id',$2,'area_id',$3,'attempt_id',$4,'correlation_key',$5))").bind(&callback.task_id).bind(&run).bind(&area).bind(&callback.attempt_id).bind(&key).fetch_one(&mut *tx).await?;
        if run != callback.run_id
            || area != callback.area_id
            || key != callback.correlation_key
            || !task_matches
        {
            record_callback_event(
                &mut tx,
                &run,
                &callback,
                &digest,
                "readiness_callback_ignored",
            )
            .await?;
            tx.commit().await?;
            return Ok(ReadinessCallbackOutcome::Ignored);
        }
        let current: Option<String> = sqlx::query_scalar(
            "SELECT current_attempt_id FROM readiness_composition_areas WHERE id=$1 FOR UPDATE",
        )
        .bind(&area)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        if current.as_deref() != Some(callback.attempt_id.as_str()) {
            record_callback_event(
                &mut tx,
                &run,
                &callback,
                &digest,
                "readiness_callback_ignored",
            )
            .await?;
            tx.commit().await?;
            return Ok(ReadinessCallbackOutcome::Ignored);
        }
        if status != "running" {
            let outcome = if old.as_deref() == Some(&digest) {
                ReadinessCallbackOutcome::Redelivered
            } else {
                record_callback_event(
                    &mut tx,
                    &run,
                    &callback,
                    &digest,
                    "readiness_callback_conflict",
                )
                .await?;
                ReadinessCallbackOutcome::Conflict
            };
            tx.commit().await?;
            return Ok(outcome);
        }
        let valid = callback.status == "succeeded" && validate_success(&callback.result).is_ok();
        let final_status = if valid {
            "succeeded"
        } else {
            match callback.status.as_str() {
                "failed" | "timed_out" | "invalid" => callback.status.as_str(),
                _ => "invalid",
            }
        };
        if valid {
            for finding in callback.result["findings"].as_array().expect("validated") {
                sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,status,severity,confidence,accepted,evidence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,true,$9)").bind(Uuid::now_v7().to_string()).bind(&run).bind(&area).bind(&callback.attempt_id).bind(finding["guardrail_key"].as_str().expect("validated")).bind(finding["status"].as_str().expect("validated")).bind(finding["severity"].as_str().expect("validated")).bind(finding["confidence"].as_f64().expect("validated")).bind(finding["evidence"].clone()).execute(&mut *tx).await?;
            }
            for suggestion in callback.result["remediation_suggestions"]
                .as_array()
                .expect("validated")
            {
                merge_suggestion(
                    &mut tx,
                    &run,
                    &area,
                    suggestion["dedupe_key"].as_str().expect("validated"),
                    suggestion,
                )
                .await?;
            }
            sqlx::query("INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ($1,$2,$3,$4)").bind(&run).bind(&area).bind(&callback.attempt_id).bind(&callback.result).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE readiness_area_attempts SET status=$1,payload_digest=$2,terminal_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$3").bind(final_status).bind(&digest).bind(&callback.attempt_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE readiness_composition_areas SET status=$1 WHERE id=$2")
            .bind(final_status)
            .bind(&area)
            .execute(&mut *tx)
            .await?;
        record_callback_event(
            &mut tx,
            &run,
            &callback,
            &digest,
            if valid {
                "readiness_result_accepted"
            } else {
                "readiness_result_terminal_failure"
            },
        )
        .await?;
        tx.commit().await?;
        Ok(ReadinessCallbackOutcome::Accepted)
    }

    pub async fn retry_area_attempt(
        &self,
        input: RetryReadinessAreaAttempt,
    ) -> Result<ReadinessAreaFanout> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let run: ReadinessRunRow = sqlx::query_as("SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at FROM readiness_runs WHERE id=$1 FOR UPDATE").bind(&input.run_id).fetch_optional(&mut *tx).await?.ok_or_else(|| Error::InvalidData("readiness run not found".into()))?;
        if !matches!(
            run.status.as_str(),
            "identifying" | "analyzing" | "aggregating"
        ) {
            return Err(Error::InvalidTransition(
                "cannot retry an area in a terminal readiness run".into(),
            ));
        }
        let area: ReadinessCompositionAreaRow = sqlx::query_as("SELECT id,run_id,area_key,composition,path_scopes,frozen_at,status FROM readiness_composition_areas WHERE id=$1 AND run_id=$2 FOR UPDATE").bind(&input.area_id).bind(&run.id).fetch_optional(&mut *tx).await?.ok_or_else(|| Error::InvalidData("readiness area not found".into()))?;
        let previous: ReadinessAreaAttemptRow = sqlx::query_as("SELECT attempt.id,attempt.run_id,attempt.area_id,attempt.attempt_number,attempt.correlation_key,attempt.status,attempt.payload_digest,attempt.created_at,attempt.terminal_at FROM readiness_area_attempts attempt JOIN readiness_composition_areas area ON area.current_attempt_id=attempt.id WHERE area.id=$1 FOR UPDATE").bind(&area.id).fetch_optional(&mut *tx).await?.ok_or_else(|| Error::InvalidData("readiness area has no current attempt".into()))?;
        if !matches!(previous.status.as_str(), "failed" | "timed_out" | "invalid") {
            return Err(Error::InvalidTransition(
                "only the current failed, timed-out, or invalid attempt may be retried".into(),
            ));
        }
        let attempt: ReadinessAreaAttemptRow = sqlx::query_as("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,$4,$5) RETURNING id,run_id,area_id,attempt_number,correlation_key,status,payload_digest,created_at,terminal_at").bind(Uuid::now_v7().to_string()).bind(&run.id).bind(&area.id).bind(previous.attempt_number + 1).bind(Uuid::now_v7().to_string()).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE readiness_composition_areas SET current_attempt_id=$1 WHERE id=$2")
            .bind(&attempt.id)
            .bind(&area.id)
            .execute(&mut *tx)
            .await?;
        let task = create_readiness_area_analysis_task_in_transaction(
            &mut tx,
            ReadinessAreaAnalysisTask {
                project_id: &run.project_id,
                creator_user_id: &input.creator_user_id,
                run_id: &run.id,
                area_id: &area.id,
                area_key: &area.area_key,
                attempt_id: &attempt.id,
                attempt_number: attempt.attempt_number,
                correlation_key: &attempt.correlation_key,
                repository_snapshot: &run.repository_snapshot,
                skill_name: &run.skill_name,
                skill_version: &run.skill_version,
                composition: &area.composition,
                path_scopes: &area.path_scopes,
            },
        )
        .await?;
        sqlx::query("UPDATE readiness_composition_areas SET status='running' WHERE id=$1")
            .bind(&area.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,'readiness_attempt_retried',$3)").bind(Uuid::now_v7().to_string()).bind(&run.id).bind(serde_json::json!({"area_id":area.id,"prior_attempt_id":previous.id,"attempt_id":attempt.id,"task_id":task.id})).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(ReadinessAreaFanout {
            area,
            attempt,
            task,
        })
    }
    /// Fence aggregation on the run row.  Every calculation below is performed
    /// from the immutable frozen area set and each area's current attempt.
    pub async fn aggregate_run(&self, run_id: &str, owner: &str) -> Result<ReadinessAggregation> {
        if owner.trim().is_empty() {
            return Err(Error::InvalidData(
                "readiness aggregation owner must be non-empty".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let run: ReadinessRunRow = sqlx::query_as("SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at FROM readiness_runs WHERE id=$1 FOR UPDATE")
            .bind(run_id).fetch_optional(&mut *tx).await?
            .ok_or_else(|| Error::InvalidData("readiness run not found".into()))?;
        if matches!(run.status.as_str(), "completed" | "completed_with_errors") {
            let area_scores = sqlx::query_as("SELECT run_id,area_id,score,applicable_weight,covered_weight,status,created_at FROM readiness_area_scores WHERE run_id=$1 ORDER BY area_id")
                .bind(run_id).fetch_all(&mut *tx).await?;
            let project_score = sqlx::query_as(
                "SELECT run_id,score,band,created_at FROM readiness_project_scores WHERE run_id=$1",
            )
            .bind(run_id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(ReadinessAggregation {
                area_scores,
                project_score,
                status: run.status,
            });
        }
        if run.status != "analyzing" && run.status != "aggregating" {
            return Err(Error::InvalidTransition(
                "readiness run is not ready for aggregation".into(),
            ));
        }
        let expected = run.expected_area_count.ok_or_else(|| {
            Error::InvalidTransition(
                "readiness aggregation requires a frozen expected area count".into(),
            )
        })?;
        let areas: Vec<(String, String)> = sqlx::query_as(
            "SELECT id,status FROM readiness_composition_areas WHERE run_id=$1 ORDER BY id FOR UPDATE",
        ).bind(run_id).fetch_all(&mut *tx).await?;
        if i32::try_from(areas.len()).ok() != Some(expected) || areas.is_empty() {
            return Err(Error::InvalidTransition(
                "frozen readiness areas do not match expected area count".into(),
            ));
        }
        let mut current = Vec::with_capacity(areas.len());
        for (area_id, area_status) in &areas {
            let attempt: Option<(String, String)> = sqlx::query_as(
                "SELECT attempt.id,attempt.status FROM readiness_area_attempts attempt JOIN readiness_composition_areas area ON area.current_attempt_id=attempt.id WHERE area.id=$1 FOR UPDATE",
            ).bind(area_id).fetch_optional(&mut *tx).await?;
            let Some((attempt_id, attempt_status)) = attempt else {
                return Err(Error::InvalidTransition(
                    "frozen readiness area has no current attempt".into(),
                ));
            };
            if !matches!(
                attempt_status.as_str(),
                "succeeded" | "failed" | "timed_out" | "invalid"
            ) || &attempt_status != area_status
            {
                return Err(Error::InvalidTransition(
                    "every current readiness attempt must be terminal".into(),
                ));
            }
            current.push((area_id.clone(), attempt_id, attempt_status));
        }
        sqlx::query("UPDATE readiness_runs SET status='aggregating',aggregation_owner=$1,aggregation_generation=aggregation_generation+1 WHERE id=$2")
            .bind(owner).bind(run_id).execute(&mut *tx).await?;
        let mut area_scores = Vec::with_capacity(current.len());
        let mut project_areas = Vec::with_capacity(current.len());
        let has_errors = current.iter().any(|(_, _, status)| status != "succeeded");
        for (area_id, attempt_id, _) in current {
            let findings: Vec<(String, String, f64)> = sqlx::query_as(
                "SELECT status,severity,confidence FROM readiness_guardrail_findings WHERE attempt_id=$1 AND accepted=true ORDER BY guardrail_key",
            ).bind(&attempt_id).fetch_all(&mut *tx).await?;
            let (score, applicable_weight, covered_weight, status) =
                readiness_area_score(&findings);
            let row: ReadinessAreaScoreRow = sqlx::query_as("INSERT INTO readiness_area_scores (run_id,area_id,score,applicable_weight,covered_weight,status) VALUES ($1,$2,$3,$4,$5,$6) RETURNING run_id,area_id,score,applicable_weight,covered_weight,status,created_at")
                .bind(run_id).bind(&area_id).bind(score).bind(applicable_weight).bind(covered_weight).bind(&status).fetch_one(&mut *tx).await?;
            project_areas.push((applicable_weight, covered_weight));
            area_scores.push(row);
        }
        let project_value = readiness_project_score(&project_areas);
        let project_score: ReadinessProjectScoreRow = sqlx::query_as("INSERT INTO readiness_project_scores (run_id,score,band) VALUES ($1,$2,$3) RETURNING run_id,score,band,created_at")
            .bind(run_id).bind(project_value).bind(readiness_score_band(project_value)).fetch_one(&mut *tx).await?;
        let terminal_status = if has_errors {
            "completed_with_errors"
        } else {
            "completed"
        };
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ($1,$2,'readiness_aggregated',$3)")
            .bind(Uuid::now_v7().to_string()).bind(run_id).bind(serde_json::json!({"owner":owner,"score":project_value,"band":project_score.band,"status":terminal_status}))
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE readiness_runs SET status=$1,completed_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$2")
            .bind(terminal_status).bind(run_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(ReadinessAggregation {
            area_scores,
            project_score,
            status: terminal_status.into(),
        })
    }
    pub async fn active_or_latest_for_project(&self, p: &str) -> Result<Option<ReadinessRunRow>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as(
            "SELECT id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,expected_area_count,created_at,completed_at \
             FROM readiness_runs WHERE project_id=$1 \
             ORDER BY CASE WHEN status IN ('identifying','analyzing','aggregating') THEN 0 ELSE 1 END, \
                      created_at DESC, id DESC LIMIT 1",
        )
        .bind(p)
        .fetch_optional(self.db.pool())
        .await
        .map_err(Into::into)
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

#[cfg(test)]
mod aggregation_tests {
    use super::{
        canonical_suggestion, readiness_area_score, readiness_project_score, readiness_score_band,
    };

    #[test]
    fn proposal_status_severity_and_confidence_rules_are_exact() {
        let findings = vec![
            ("covered".into(), "critical".into(), 0.9),
            ("partial".into(), "high".into(), 1.0),
            ("covered".into(), "medium".into(), 0.69),
            ("missing".into(), "low".into(), 1.0),
            ("unknown".into(), "high".into(), 1.0),
            ("analysis_error".into(), "medium".into(), 1.0),
            ("unsupported".into(), "critical".into(), 1.0),
        ];
        let (score, applicable, covered, status) = readiness_area_score(&findings);
        assert_eq!(
            (applicable, covered, status.as_str()),
            (16, 7.5, "supported")
        );
        assert!((score - 7.5 / 16.0).abs() < f64::EPSILON);
        assert_eq!(
            readiness_area_score(&[("unsupported".into(), "high".into(), 1.0)]).3,
            "unsupported"
        );
    }

    #[test]
    fn proposal_project_score_is_applicable_weighted_and_excludes_unsupported_areas() {
        assert!(
            (readiness_project_score(&[(8, 6.0), (2, 1.0), (0, 0.0)]) - 0.7).abs() < f64::EPSILON
        );
        assert_eq!(readiness_project_score(&[(0, 0.0)]), 0.0);
    }

    #[test]
    fn proposal_score_band_boundaries_are_exact() {
        assert_eq!(readiness_score_band(0.3999), "blocked");
        assert_eq!(readiness_score_band(0.40), "emerging");
        assert_eq!(readiness_score_band(0.6999), "emerging");
        assert_eq!(readiness_score_band(0.70), "ready");
        assert_eq!(readiness_score_band(0.8499), "ready");
        assert_eq!(readiness_score_band(0.85), "strong");
        assert_eq!(readiness_score_band(1.0), "strong");
    }

    #[test]
    fn canonical_suggestion_is_identical_when_callback_arrival_is_reversed() {
        let from_area_a = serde_json::json!({
            "dedupe_key": "auth",
            "action": "rotate credentials",
            "guardrail_id": "z",
            "guardrail_ids": ["m", "z"],
            "area_id": "area-a",
            "area_ids": ["area-shared", "area-a"]
        });
        let from_area_b = serde_json::json!({
            "dedupe_key": "auth",
            "action": "rotate credentials",
            "guardrail_id": "a",
            "guardrail_ids": ["m", "a"],
            "area_id": "area-b",
            "area_ids": ["area-b", "area-shared"]
        });
        let a_then_b = canonical_suggestion(
            &from_area_b,
            "area-b",
            Some(&canonical_suggestion(&from_area_a, "area-a", None)),
        );
        let b_then_a = canonical_suggestion(
            &from_area_a,
            "area-a",
            Some(&canonical_suggestion(&from_area_b, "area-b", None)),
        );

        assert_eq!(a_then_b, b_then_a);
        assert_eq!(
            a_then_b,
            serde_json::json!({
                "dedupe_key": "auth",
                "action": "rotate credentials",
                "area_ids": ["area-a", "area-b", "area-shared"],
                "guardrail_ids": ["a", "m", "z"]
            })
        );
    }
}
