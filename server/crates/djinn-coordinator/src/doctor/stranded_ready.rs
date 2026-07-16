//! Cheap doctor check for stranded-ready dispatch starvation.
//!
//! Reuses the DB stranded-ready contract from `djinn_db::TaskRepository::board_health`
//! (delivered by task `lke3`). The check is read-only: it snapshots the `stranded_ready`
//! board-health section and emits one [`Finding`] per stranded task, preserving the
//! threshold, severity, age, and dispatch-gate evidence already computed by the DB.

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

pub const STRANDED_READY_CHECK_NAME: &str = "stranded_ready";
pub const DEFAULT_WARNING_MINUTES: i64 = 30;
pub const DEFAULT_ERROR_MINUTES: i64 = 60;
pub const DEFAULT_CRITICAL_MINUTES: i64 = 180;

/// Source of the stranded-ready board-health snapshot.
///
/// Production code queries the DB once and hands the resulting JSON to the check.
/// Tests provide an in-memory double so the resolver/severity logic is hermetic.
pub trait StrandedReadySource: Send + Sync {
    /// Return the raw `stranded_ready` section value produced by the DB contract.
    fn snapshot(&self) -> serde_json::Value;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrandedReadyCandidate {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    pub owner: String,
    pub epic_short_id: Option<String>,
    pub unclaimed_since: String,
    pub unclaimed_since_confidence: String,
    pub elapsed_minutes: i64,
    pub severity: String,
    pub threshold: serde_json::Value,
    pub dispatch_gate: serde_json::Value,
}

impl StrandedReadyCandidate {
    fn try_from_finding(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            short_id: value.get("short_id")?.as_str()?.to_owned(),
            title: value.get("title")?.as_str()?.to_owned(),
            status: value.get("status")?.as_str()?.to_owned(),
            owner: value.get("owner")?.as_str()?.to_owned(),
            epic_short_id: value
                .get("epic_short_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            unclaimed_since: value.get("unclaimed_since")?.as_str()?.to_owned(),
            unclaimed_since_confidence: value
                .get("unclaimed_since_confidence")?
                .as_str()?
                .to_owned(),
            elapsed_minutes: value.get("elapsed_minutes")?.as_i64()?,
            severity: value.get("severity")?.as_str()?.to_owned(),
            threshold: value.get("threshold")?.clone(),
            dispatch_gate: value.get("dispatch_gate")?.clone(),
        })
    }
}

fn severity_from_thresholds(
    elapsed_minutes: i64,
    warning: i64,
    error: i64,
    critical: i64,
) -> FindingSeverity {
    if elapsed_minutes >= critical {
        FindingSeverity::Critical
    } else if elapsed_minutes >= error {
        FindingSeverity::Error
    } else if elapsed_minutes >= warning {
        FindingSeverity::Warn
    } else {
        FindingSeverity::Info
    }
}

fn parse_threshold(threshold: &serde_json::Value, key: &str, default: i64) -> i64 {
    threshold
        .get(key)
        .and_then(|v| v.as_i64())
        .unwrap_or(default)
}

/// Cheap, read-only doctor check for stranded-ready tasks.
pub struct StrandedReadyCheck {
    source: Arc<dyn StrandedReadySource>,
}

impl StrandedReadyCheck {
    pub fn new(source: Arc<dyn StrandedReadySource>) -> Self {
        Self { source }
    }

    fn finding_for(candidate: StrandedReadyCandidate) -> Option<Finding> {
        let warning_minutes = parse_threshold(
            &candidate.threshold,
            "warning_minutes",
            DEFAULT_WARNING_MINUTES,
        );
        let error_minutes =
            parse_threshold(&candidate.threshold, "error_minutes", DEFAULT_ERROR_MINUTES);
        let critical_minutes = parse_threshold(
            &candidate.threshold,
            "critical_minutes",
            DEFAULT_CRITICAL_MINUTES,
        );
        let severity = severity_from_thresholds(
            candidate.elapsed_minutes,
            warning_minutes,
            error_minutes,
            critical_minutes,
        );
        let severity_label = severity.as_str();

        // Defence in depth: the DB contract is expected to exclude tasks whose
        // dispatch gate would allow a normal dispatch. If a non-stranded verdict
        // leaks through, skip it so the doctor check stays read-only and does not
        // flag tasks that are gated for legitimate reasons.
        let gate_verdict = candidate
            .dispatch_gate
            .get("gate_verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("stranded");
        if gate_verdict != "stranded" {
            return None;
        }

        let inputs = json!({
            "id": candidate.id,
            "short_id": candidate.short_id,
            "status": candidate.status,
            "elapsed_minutes": candidate.elapsed_minutes,
            "threshold": candidate.threshold,
            "dispatch_gate": candidate.dispatch_gate,
        });
        let outputs = json!({
            "is_stranded": true,
            "severity": severity_label,
            "reason": "stranded_ready",
        });
        let snapshot =
            ResolverSnapshot::new("resolve_stranded_ready", inputs.clone(), outputs.clone());
        let detail = format!(
            "task {} ({}) has been stranded-ready for {} minutes (severity: {})",
            candidate.short_id, candidate.title, candidate.elapsed_minutes, severity_label
        );
        let evidence = json!({
            "task_id": candidate.id,
            "short_id": candidate.short_id,
            "title": candidate.title,
            "status": candidate.status,
            "owner": candidate.owner,
            "epic_short_id": candidate.epic_short_id,
            "unclaimed_since": candidate.unclaimed_since,
            "unclaimed_since_confidence": candidate.unclaimed_since_confidence,
            "elapsed_minutes": candidate.elapsed_minutes,
            "severity": severity_label,
            "threshold": candidate.threshold,
            "dispatch_gate": candidate.dispatch_gate,
        });

        Some(
            Finding::new(severity, STRANDED_READY_CHECK_NAME, snapshot, detail)
                .with_entity_id("task_id", candidate.id)
                .with_entity_id("short_id", candidate.short_id)
                .with_evidence(evidence),
        )
    }

    fn attribution_finding_for(raw: &serde_json::Value) -> Option<Finding> {
        let reason = raw.get("reason")?.as_str()?;
        if !matches!(
            reason,
            "legacy_null_creator" | "unresolvable_creation_provenance"
        ) || raw.get("gate_verdict").and_then(|value| value.as_str()) != Some("blocked")
        {
            return None;
        }
        let id = raw.get("id")?.as_str()?.to_owned();
        let short_id = raw.get("short_id")?.as_str()?.to_owned();
        let title = raw.get("title")?.as_str()?.to_owned();
        let status = raw.get("status")?.as_str()?.to_owned();
        let provenance = raw.get("creation_provenance")?.clone();
        let inputs = json!({
            "id": id.clone(),
            "short_id": short_id.clone(),
            "status": status.clone(),
            "reason": reason,
            "gate_verdict": "blocked",
            "creation_provenance": provenance.clone(),
        });
        let snapshot = ResolverSnapshot::new(
            "resolve_task_creator_attribution",
            inputs,
            json!({ "dispatchable": false, "reason": reason }),
        );
        let evidence = json!({
            "task_id": id,
            "short_id": short_id,
            "title": title,
            "status": status,
            "reason": reason,
            "gate_verdict": "blocked",
            "creation_provenance": provenance,
        });
        Some(
            Finding::new(
                FindingSeverity::Error,
                STRANDED_READY_CHECK_NAME,
                snapshot,
                format!("task {short_id} ({title}) has blocked creation attribution: {reason}"),
            )
            .with_entity_id("task_id", id)
            .with_entity_id("short_id", short_id)
            .with_evidence(evidence),
        )
    }
}

impl DoctorCheck for StrandedReadyCheck {
    fn name(&self) -> &'static str {
        STRANDED_READY_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Flags ready/dispatchable tasks that have been unclaimed beyond the stranded-ready threshold"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let snapshot = self.source.snapshot();
        let stranded_snapshot = snapshot.get("stranded_ready").unwrap_or(&snapshot);
        let findings_array = stranded_snapshot
            .get("findings")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        let mut findings = Vec::new();
        for raw in findings_array {
            match StrandedReadyCandidate::try_from_finding(raw) {
                Some(candidate) => {
                    if let Some(finding) = Self::finding_for(candidate) {
                        findings.push(finding);
                    }
                }
                None => {
                    warn!(
                        raw = %raw,
                        "stranded_ready doctor: skipping malformed stranded-ready candidate"
                    );
                }
            }
        }
        let attribution_findings = snapshot
            .get("attribution_findings")
            .and_then(|section| section.get("findings"))
            .and_then(|findings| findings.as_array())
            .map(|findings| findings.as_slice())
            .unwrap_or(&[]);
        for raw in attribution_findings {
            match Self::attribution_finding_for(raw) {
                Some(finding) => findings.push(finding),
                None => warn!(
                    raw = %raw,
                    "stranded_ready doctor: skipping malformed attribution candidate"
                ),
            }
        }
        Ok(findings)
    }
}

/// In-memory source for tests.
#[derive(Clone, Debug, Default)]
pub struct MemoryStrandedReadySource {
    pub snapshot: serde_json::Value,
}

impl MemoryStrandedReadySource {
    pub fn new(snapshot: serde_json::Value) -> Self {
        Self { snapshot }
    }
}

impl StrandedReadySource for MemoryStrandedReadySource {
    fn snapshot(&self) -> serde_json::Value {
        self.snapshot.clone()
    }
}

/// Production source backed by a `TaskRepository::board_health` query.
///
/// The DB query is async, but the [`DoctorCheck::run`] seam is synchronous.
/// This source bridges the two worlds by keeping a cached snapshot that is
/// refreshed asynchronously by the coordinator tick; the synchronous `snapshot()`
/// method reads the last cached value without blocking.
#[derive(Clone)]
pub struct TaskRepositoryStrandedReadySource {
    db: djinn_db::Database,
    events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    cache: Arc<tokio::sync::RwLock<serde_json::Value>>,
}

impl TaskRepositoryStrandedReadySource {
    pub fn new(
        db: djinn_db::Database,
        events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    ) -> Self {
        Self {
            db,
            events_tx,
            cache: Arc::new(tokio::sync::RwLock::new(json!({}))),
        }
    }

    /// Refresh the cached snapshot from the DB. The coordinator tick calls this
    /// once before running the cheap doctor subset so the check sees a fresh,
    /// bounded view of stranded-ready tasks.
    pub async fn refresh(&self) {
        let task_repo = djinn_db::TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let snapshot = match task_repo.board_health(30).await {
            Ok(report) => json!({
                "stranded_ready": report.get("stranded_ready").cloned().unwrap_or_else(|| json!({})),
                "attribution_findings": report.get("attribution_findings").cloned().unwrap_or_else(|| json!({})),
            }),
            Err(error) => {
                warn!(
                    error = %error,
                    "stranded_ready doctor: failed to load board_health snapshot"
                );
                json!({})
            }
        };
        let mut guard = self.cache.write().await;
        *guard = snapshot;
    }
}

impl StrandedReadySource for TaskRepositoryStrandedReadySource {
    fn snapshot(&self) -> serde_json::Value {
        // Best-effort read of the last refreshed snapshot. If the cache is
        // contended with a writer, fall back to an empty snapshot so the cheap
        // tick never blocks on the refresh task.
        match self.cache.try_read() {
            Ok(guard) => guard.clone(),
            Err(_) => {
                warn!("stranded_ready doctor: cache locked during snapshot read; returning empty");
                json!({})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn candidate_json(overrides: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("id".to_owned(), json!("task-id-1"));
        map.insert("short_id".to_owned(), json!("task-1"));
        map.insert("title".to_owned(), json!("Stranded task"));
        map.insert("status".to_owned(), json!("open"));
        map.insert("owner".to_owned(), json!("owner-1"));
        map.insert("epic_short_id".to_owned(), json!("ep01"));
        map.insert(
            "unclaimed_since".to_owned(),
            json!("2026-01-01T00:00:00.000Z"),
        );
        map.insert("unclaimed_since_confidence".to_owned(), json!("high"));
        map.insert("elapsed_minutes".to_owned(), json!(45));
        map.insert("severity".to_owned(), json!("warn"));
        map.insert(
            "threshold".to_owned(),
            json!({"warning_minutes": 30, "error_minutes": 60, "critical_minutes": 180}),
        );
        map.insert(
            "dispatch_gate".to_owned(),
            json!({
                "evaluated_role": "worker",
                "toolset": ["task_edit"],
                "model_requirement": "provider/model-a",
                "image_ready": true,
                "breaker_open": false,
                "manually_paused": false,
                "rate_limited": false,
                "credential_available": true,
                "gate_verdict": "stranded",
                "reasons": [],
            }),
        );
        for (k, v) in overrides {
            map.insert(k, v);
        }
        serde_json::Value::Object(map)
    }

    fn snapshot_with(findings: Vec<serde_json::Value>) -> serde_json::Value {
        json!({
            "total": findings.len(),
            "threshold_minutes": 30,
            "findings": findings,
        })
    }

    #[test]
    fn check_is_cheap_and_named() {
        let check = StrandedReadyCheck::new(Arc::new(MemoryStrandedReadySource::default()));
        assert_eq!(check.name(), STRANDED_READY_CHECK_NAME);
        assert_eq!(check.cadence(), DoctorCheckCadence::Cheap);
    }

    #[test]
    fn warning_finding_includes_threshold_and_gate_evidence() {
        let source = Arc::new(MemoryStrandedReadySource::new(snapshot_with(vec![
            candidate_json(serde_json::Map::new()),
        ])));
        let findings = StrandedReadyCheck::new(source).run().expect("run");
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.check_name, STRANDED_READY_CHECK_NAME);
        assert_eq!(finding.severity, FindingSeverity::Warn);
        assert_eq!(
            finding.entity_ids.get("task_id").map(String::as_str),
            Some("task-id-1")
        );
        assert_eq!(
            finding.entity_ids.get("short_id").map(String::as_str),
            Some("task-1")
        );
        assert_eq!(finding.evidence["elapsed_minutes"], 45);
        assert_eq!(finding.evidence["severity"], "warn");
        assert_eq!(finding.evidence["threshold"]["warning_minutes"], 30);
        assert_eq!(finding.evidence["threshold"]["error_minutes"], 60);
        assert_eq!(finding.evidence["threshold"]["critical_minutes"], 180);
        assert_eq!(
            finding.evidence["dispatch_gate"]["evaluated_role"],
            "worker"
        );
        assert_eq!(
            finding.evidence["dispatch_gate"]["gate_verdict"],
            "stranded"
        );
        assert_eq!(finding.resolver_snapshot.resolver, "resolve_stranded_ready");
        assert_eq!(finding.resolver_snapshot.outputs["severity"], "warn");
        assert!(finding.detail.contains("task-1") && finding.detail.contains("45 minutes"));
    }

    #[test]
    fn error_severity_is_derived() {
        let mut overrides = serde_json::Map::new();
        overrides.insert("elapsed_minutes".to_owned(), json!(75));
        overrides.insert("severity".to_owned(), json!("error"));
        let source = Arc::new(MemoryStrandedReadySource::new(snapshot_with(vec![
            candidate_json(overrides),
        ])));
        let findings = StrandedReadyCheck::new(source).run().expect("run");
        assert_eq!(findings[0].severity, FindingSeverity::Error);
        assert_eq!(findings[0].evidence["severity"], "error");
    }

    #[test]
    fn critical_severity_is_derived() {
        let mut overrides = serde_json::Map::new();
        overrides.insert("elapsed_minutes".to_owned(), json!(200));
        overrides.insert("severity".to_owned(), json!("critical"));
        let source = Arc::new(MemoryStrandedReadySource::new(snapshot_with(vec![
            candidate_json(overrides),
        ])));
        let findings = StrandedReadyCheck::new(source).run().expect("run");
        assert_eq!(findings[0].severity, FindingSeverity::Critical);
        assert_eq!(findings[0].evidence["severity"], "critical");
    }

    #[test]
    fn excluded_gated_task_produces_no_finding() {
        let source = Arc::new(MemoryStrandedReadySource::new(snapshot_with(Vec::new())));
        let findings = StrandedReadyCheck::new(source).run().expect("run");
        assert!(findings.is_empty());
    }

    #[test]
    fn non_stranded_gate_verdict_is_skipped() {
        let mut overrides = serde_json::Map::new();
        overrides.insert(
            "dispatch_gate".to_owned(),
            json!({
                "evaluated_role": "worker",
                "toolset": ["task_edit"],
                "model_requirement": "provider/model-a",
                "image_ready": true,
                "breaker_open": false,
                "manually_paused": false,
                "rate_limited": false,
                "credential_available": true,
                "gate_verdict": "not_stranded",
                "reasons": [],
            }),
        );
        let source = Arc::new(MemoryStrandedReadySource::new(snapshot_with(vec![
            candidate_json(overrides),
        ])));
        let findings = StrandedReadyCheck::new(source).run().expect("run");
        assert!(
            findings.is_empty(),
            "a candidate with a non-stranded gate verdict must be skipped"
        );
    }

    #[test]
    fn blocked_attribution_finding_is_reported_without_stranded_age() {
        let source = Arc::new(MemoryStrandedReadySource::new(json!({
            "stranded_ready": snapshot_with(Vec::new()),
            "attribution_findings": {
                "findings": [{
                    "id": "task-id-legacy",
                    "short_id": "legacy",
                    "title": "Legacy task",
                    "status": "open",
                    "reason": "legacy_null_creator",
                    "gate_verdict": "blocked",
                    "creation_provenance": {
                        "source": "tasks.created_by_user_id",
                        "creator_user_id": null,
                        "creator_resolved": false,
                        "resolution": "unresolved",
                    },
                }],
            },
        })));
        let findings = StrandedReadyCheck::new(source).run().expect("run");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Error);
        assert_eq!(findings[0].resolver_snapshot.outputs["reason"], "legacy_null_creator");
        assert_eq!(findings[0].evidence["gate_verdict"], "blocked");
        assert_eq!(findings[0].evidence["creation_provenance"]["creator_resolved"], false);
    }

    #[test]
    fn malformed_candidate_is_skipped() {
        let source = Arc::new(MemoryStrandedReadySource::new(json!({
            "total": 1,
            "findings": [{"id": "broken"}],
        })));
        let findings = StrandedReadyCheck::new(source).run().expect("run");
        assert!(findings.is_empty());
    }
}
