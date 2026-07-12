//! Read-only doctor check for historical closed-parent/open-child drift.
//!
//! This check consumes the additive `board_health.closed_parent_open_children`
//! contract rather than reimplementing the parent-disposition matrix. Every
//! finding persists the complete board-health child snapshot and the selected
//! `recommended_disposition` matrix row, so a later opt-in repair can safely
//! use the exact evidence observed by this dry-run check.

use std::sync::Arc;

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};
use serde_json::json;
use tracing::warn;

pub const CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME: &str = "closed_parent_open_children";

/// Source of the read-only closed-parent orphan board-health snapshot.
pub trait ClosedParentOpenChildrenSource: Send + Sync {
    /// Return the raw `closed_parent_open_children` section from board health.
    fn snapshot(&self) -> serde_json::Value;
}

/// Read-only doctor check that persists the board-health evidence verbatim.
pub struct ClosedParentOpenChildrenCheck {
    source: Arc<dyn ClosedParentOpenChildrenSource>,
}

impl ClosedParentOpenChildrenCheck {
    pub fn new(source: Arc<dyn ClosedParentOpenChildrenSource>) -> Self {
        Self { source }
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
                ResolverSnapshot::new(
                    "resolve_closed_parent_open_children",
                    inputs,
                    outputs,
                ),
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
                warn!("closed_parent_open_children doctor: cache locked during snapshot read; returning empty");
                json!({})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(finding.evidence["board_health_finding"]["terminal_epic_ids"][0], "epic-1");
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
            assert_eq!(finding.resolver_snapshot.outputs["would_mutate"], action != "retain");
        }
    }
}
