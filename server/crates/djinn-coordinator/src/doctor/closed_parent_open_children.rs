//! Read-only doctor check for historical closed-parent/open-child drift.
//!
//! This check consumes the additive `board_health.closed_parent_open_children`
//! contract rather than reimplementing the parent-disposition matrix. Every
//! finding persists the complete board-health child snapshot and the selected
//! `recommended_disposition` matrix row, so a later opt-in repair can safely
//! use the exact evidence observed by this dry-run check.

use std::sync::Arc;

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorError, DoctorResult, Finding, FindingSeverity,
    ResolverSnapshot,
};
use serde_json::json;
use tracing::warn;

pub const CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME: &str = "closed_parent_open_children";

/// Source of the read-only closed-parent orphan board-health snapshot.
pub trait ClosedParentOpenChildrenSource: Send + Sync {
    /// Return the raw `closed_parent_open_children` section from board health.
    fn snapshot(&self) -> serde_json::Value;

    /// Refresh immediately before a synchronous doctor run.
    ///
    /// In-memory sources are already current. The production implementation
    /// overrides this so direct MCP runs cannot observe a prior tick's cache.
    fn refresh_for_run(&self) {}
}

/// Opt-in repair source for the closed-parent orphan doctor fix.
///
/// The production implementation wraps a database transaction that locks the
/// target task, re-runs the shared classifier under the lock, and applies the
/// safe mutation subset. In-memory test doubles return a fabricated outcome.
pub trait ClosedParentOpenChildrenRepairSource: Send + Sync {
    /// Apply the transactional repair for one finding.
    ///
    /// `terminal_epic_ids` and `terminal_proposal_ids` are taken from the
    /// board-health finding so the repair can reconstruct the correct scope.
    /// Returns the outcome (applied or skipped) so the caller can surface it.
    fn repair(
        &self,
        task_id: &str,
        snapshot_status: &str,
        snapshot_action: &str,
        terminal_epic_ids: &[String],
        terminal_proposal_ids: &[String],
    ) -> Result<djinn_db::repositories::task::DoctorRepairOutcome, String>;
}

/// Read-only doctor check that persists the board-health evidence verbatim.
/// When a [`ClosedParentOpenChildrenRepairSource`] is attached, `fix()`
/// performs the opt-in mutating repair.
pub struct ClosedParentOpenChildrenCheck {
    source: Arc<dyn ClosedParentOpenChildrenSource>,
    repair_source: Option<Arc<dyn ClosedParentOpenChildrenRepairSource>>,
}

impl ClosedParentOpenChildrenCheck {
    pub fn new(source: Arc<dyn ClosedParentOpenChildrenSource>) -> Self {
        Self {
            source,
            repair_source: None,
        }
    }

    /// Attach a repair source so `fix()` can perform the mutating repair.
    pub fn with_repair_source(
        mut self,
        repair_source: Arc<dyn ClosedParentOpenChildrenRepairSource>,
    ) -> Self {
        self.repair_source = Some(repair_source);
        self
    }

    fn finding_for(raw: &serde_json::Value) -> Option<Finding> {
        let task_id = raw.get("id")?.as_str()?.to_owned();
        let short_id = raw.get("short_id")?.as_str()?.to_owned();
        let title = raw.get("title")?.as_str()?.to_owned();
        let status = raw.get("status")?.as_str()?.to_owned();
        let disposition = raw.get("recommended_disposition")?.clone();
        let action = disposition.get("action")?.as_str()?;
        let target_status = disposition.get("status")?.as_str()?;
        let guard = disposition.get("guard")?.as_str()?;

        // The board-health query is the shared disposition matrix authority.
        // Persist its selected row rather than classifying from status here.
        let inputs = json!({ "board_health_finding": raw });
        let outputs = json!({
            "selected_disposition": disposition,
            "would_mutate": matches!(action, "close" | "park"),
        });
        let detail = format!(
            "closed-parent orphan {} ({}) is '{}' — dry-run selects {} to '{}' ({})",
            short_id, title, status, action, target_status, guard
        );

        Some(
            Finding::new(
                FindingSeverity::Warn,
                CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                ResolverSnapshot::new("resolve_closed_parent_open_children", inputs, outputs),
                detail,
            )
            .with_entity_id("task_id", task_id)
            .with_entity_id("short_id", short_id)
            .with_evidence(json!({
                "board_health_finding": raw,
                "selected_disposition": disposition,
            })),
        )
    }
}

impl DoctorCheck for ClosedParentOpenChildrenCheck {
    fn name(&self) -> &'static str {
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Reports historical open children of terminal parents with the shared disposition matrix row; read-only dry-run"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        // Keep direct MCP runs current as well as the periodic cheap run.
        // Refreshing this adapter is read-only and never invokes repair.
        self.source.refresh_for_run();
        let snapshot = self.source.snapshot();
        let findings = snapshot
            .get("findings")
            .and_then(|value| value.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        Ok(findings
            .iter()
            .filter_map(|raw| match Self::finding_for(raw) {
                Some(finding) => Some(finding),
                None => {
                    warn!(raw = %raw, "closed_parent_open_children doctor: skipping malformed board-health finding");
                    None
                }
            })
            .collect())
    }

    fn fix(&self, finding: &Finding) -> DoctorResult<()> {
        let Some(repair_source) = &self.repair_source else {
            return Err(DoctorError::FixNotSupported {
                check: self.name().to_string(),
            });
        };

        // Extract the task_id and snapshot evidence from the persisted finding.
        // The finding's resolver_snapshot.inputs carries the full board-health
        // child snapshot (status, recommended_disposition, terminal_epic_ids,
        // etc.). The evidence also carries the same data.
        let task_id = finding
            .entity_ids
            .get("task_id")
            .ok_or_else(|| {
                DoctorError::InvalidInput(
                    "closed_parent_open_children fix: finding missing task_id entity".to_string(),
                )
            })?
            .clone();

        let board_health_finding = &finding.evidence["board_health_finding"];
        let snapshot_status = board_health_finding
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let snapshot_action = board_health_finding
            .get("recommended_disposition")
            .and_then(|d| d.get("action"))
            .and_then(|v| v.as_str())
            .unwrap_or("retain");

        let terminal_epic_ids = extract_string_array(board_health_finding, "terminal_epic_ids");
        let terminal_proposal_ids =
            extract_string_array(board_health_finding, "terminal_proposal_ids");

        repair_source
            .repair(
                &task_id,
                snapshot_status,
                snapshot_action,
                &terminal_epic_ids,
                &terminal_proposal_ids,
            )
            .map(|_| ())
            .map_err(DoctorError::Backend)
    }
}

/// Extract string values from a JSON array field.
fn extract_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// In-memory source for unit tests.
#[derive(Clone, Debug, Default)]
pub struct MemoryClosedParentOpenChildrenSource {
    pub snapshot: serde_json::Value,
}

impl MemoryClosedParentOpenChildrenSource {
    pub fn new(snapshot: serde_json::Value) -> Self {
        Self { snapshot }
    }
}

impl ClosedParentOpenChildrenSource for MemoryClosedParentOpenChildrenSource {
    fn snapshot(&self) -> serde_json::Value {
        self.snapshot.clone()
    }
}

/// Production source backed by the cached board-health report.
#[derive(Clone)]
pub struct TaskRepositoryClosedParentOpenChildrenSource {
    db: djinn_db::Database,
    events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    cache: Arc<tokio::sync::RwLock<serde_json::Value>>,
}

impl TaskRepositoryClosedParentOpenChildrenSource {
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

    /// Refresh the cached read-only board-health section before cheap checks run.
    pub async fn refresh(&self) {
        let task_repo = djinn_db::TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let snapshot = match task_repo.board_health(30).await {
            Ok(report) => report
                .get("closed_parent_open_children")
                .cloned()
                .unwrap_or_else(|| json!({})),
            Err(error) => {
                warn!(error = %error, "closed_parent_open_children doctor: failed to load board-health snapshot");
                json!({})
            }
        };
        *self.cache.write().await = snapshot;
    }
}

impl ClosedParentOpenChildrenSource for TaskRepositoryClosedParentOpenChildrenSource {
    fn snapshot(&self) -> serde_json::Value {
        match self.cache.try_read() {
            Ok(guard) => guard.clone(),
            Err(_) => {
                warn!(
                    "closed_parent_open_children doctor: cache locked during snapshot read; returning empty"
                );
                json!({})
            }
        }
    }

    fn refresh_for_run(&self) {
        let source = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(source.refresh()));
            }
            Ok(_) => {
                let _ = std::thread::spawn(move || {
                    match tokio::runtime::Runtime::new() {
                        Ok(runtime) => runtime.block_on(source.refresh()),
                        Err(error) => warn!(%error, "closed_parent_open_children doctor: failed to create refresh runtime"),
                    }
                })
                .join();
            }
            Err(_) => match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime.block_on(source.refresh()),
                Err(error) => {
                    warn!(%error, "closed_parent_open_children doctor: failed to create refresh runtime")
                }
            },
        }
    }
}

impl ClosedParentOpenChildrenRepairSource for TaskRepositoryClosedParentOpenChildrenSource {
    fn repair(
        &self,
        task_id: &str,
        snapshot_status: &str,
        snapshot_action: &str,
        terminal_epic_ids: &[String],
        terminal_proposal_ids: &[String],
    ) -> Result<djinn_db::repositories::task::DoctorRepairOutcome, String> {
        let db = self.db.clone();
        let task_id = task_id.to_owned();
        let snapshot_status = snapshot_status.to_owned();
        let snapshot_action = snapshot_action.to_owned();
        let terminal_epic_ids = terminal_epic_ids.to_owned();
        let terminal_proposal_ids = terminal_proposal_ids.to_owned();

        let run = async move {
            let mut tx = db.pool().begin().await.map_err(|e| e.to_string())?;
            let outcome = djinn_db::repositories::task::apply_doctor_repair_tx(
                &mut tx,
                &task_id,
                &snapshot_status,
                &snapshot_action,
                &terminal_epic_ids,
                &terminal_proposal_ids,
            )
            .await
            .map_err(|e| e.to_string())?;
            tx.commit().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(outcome)
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(run))
            }
            Ok(_) => {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let result = match tokio::runtime::Runtime::new() {
                        Ok(runtime) => runtime.block_on(run),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = tx.send(result);
                });
                rx.recv().map_err(|e| e.to_string())?
            }
            Err(_) => match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime.block_on(run),
                Err(error) => Err(error.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::doctor::{DoctorRegistry, doctor_run};

    fn snapshot(action: &str, status: &str, guard: &str) -> serde_json::Value {
        json!({"total": 1, "findings": [{
            "id": "task-1", "short_id": "t1", "title": "Child", "status": status,
            "terminal_epic_ids": ["epic-1"], "terminal_proposal_ids": [],
            "other_open_parent_ids": [], "external_open_dependents": [],
            "recommended_disposition": {"action": action, "status": if action == "close" { "closed" } else if action == "park" { "needs_lead_intervention" } else { "none" }, "guard": guard}
        }]})
    }

    #[test]
    fn persists_health_snapshot_and_close_matrix_row() {
        let check = ClosedParentOpenChildrenCheck::new(Arc::new(
            MemoryClosedParentOpenChildrenSource::new(snapshot("close", "open", "parent_closed")),
        ));
        let finding = check.run().unwrap().pop().unwrap();
        assert_eq!(finding.check_name, CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME);
        assert_eq!(
            finding.evidence["board_health_finding"]["terminal_epic_ids"][0],
            "epic-1"
        );
        assert_eq!(finding.evidence["selected_disposition"]["action"], "close");
        assert_eq!(finding.resolver_snapshot.outputs["would_mutate"], true);
    }

    #[test]
    fn distinguishes_historical_parks_and_guarded_skip() {
        for (action, status, guard) in [
            ("park", "in_progress", "historical_parent_closed_in_flight"),
            ("park", "pr_review", "historical_parent_closed_pr_active"),
            ("retain", "open", "external_open_dependent"),
        ] {
            let check = ClosedParentOpenChildrenCheck::new(Arc::new(
                MemoryClosedParentOpenChildrenSource::new(snapshot(action, status, guard)),
            ));
            let finding = check.run().unwrap().pop().unwrap();
            assert_eq!(finding.evidence["selected_disposition"]["guard"], guard);
            assert_eq!(
                finding.resolver_snapshot.outputs["would_mutate"],
                action != "retain"
            );
        }
    }

    #[test]
    fn registered_check_runs_by_name_and_returns_serialized_evidence() {
        let registry = DoctorRegistry::new();
        let source = Arc::new(MemoryClosedParentOpenChildrenSource::new(snapshot(
            "close",
            "open",
            "parent_closed",
        )));
        crate::doctor::register_closed_parent_open_children_check(&registry, source);

        let results = doctor_run(&registry, Some(&[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]))
            .expect("registered named check runs");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME);
        let finding = results[0].1.first().expect("serialized finding");
        assert_eq!(finding.entity_ids["task_id"], "task-1");
        assert_eq!(finding.evidence["board_health_finding"]["id"], "task-1");
        assert_eq!(
            finding.evidence["selected_disposition"],
            json!({"action":"close", "status":"closed", "guard":"parent_closed"})
        );
    }
}
