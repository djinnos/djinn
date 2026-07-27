//! Read-only doctor check for open epics with no dispatchable work.

use std::sync::Arc;

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorError, DoctorResult, Finding, FindingSeverity,
    ResolverSnapshot,
};
use serde_json::json;
use tracing::warn;

pub const STALLED_EPIC_CHECK_NAME: &str = "stalled_epic";

pub trait StalledEpicSource: Send + Sync {
    fn snapshot(&self) -> serde_json::Value;
    fn refresh_for_run(&self) {}
}

pub struct StalledEpicCheck {
    source: Arc<dyn StalledEpicSource>,
}

impl StalledEpicCheck {
    pub fn new(source: Arc<dyn StalledEpicSource>) -> Self {
        Self { source }
    }
}

impl DoctorCheck for StalledEpicCheck {
    fn name(&self) -> &'static str {
        STALLED_EPIC_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Reports open epics with no active planning task and no dispatchable worker work"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::OnDemand
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        self.source.refresh_for_run();
        let snapshot = self.source.snapshot();
        Ok(snapshot["findings"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter_map(|raw| {
                let epic_id = raw["id"].as_str()?.to_owned();
                let short_id = raw["short_id"].as_str()?.to_owned();
                let title = raw["title"].as_str()?.to_owned();
                Some(
                    Finding::new(
                        FindingSeverity::Warn,
                        STALLED_EPIC_CHECK_NAME,
                        ResolverSnapshot::new(
                            "report_stalled_epic",
                            json!({"epic": raw}),
                            json!({"would_mutate": false}),
                        ),
                        format!(
                            "open epic {short_id} ({title}) has no active planning task and no dispatchable worker work"
                        ),
                    )
                    .with_entity_id("epic_id", epic_id)
                    .with_entity_id("short_id", short_id)
                    .with_evidence(json!({"epic": raw})),
                )
            })
            .collect())
    }

    fn fix(&self, _finding: &Finding) -> DoctorResult<()> {
        Err(DoctorError::FixNotSupported {
            check: self.name().to_string(),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemoryStalledEpicSource {
    snapshot: serde_json::Value,
}

impl MemoryStalledEpicSource {
    pub fn new(snapshot: serde_json::Value) -> Self {
        Self { snapshot }
    }
}

impl StalledEpicSource for MemoryStalledEpicSource {
    fn snapshot(&self) -> serde_json::Value {
        self.snapshot.clone()
    }
}

#[derive(Clone)]
pub struct TaskRepositoryStalledEpicSource {
    db: djinn_db::Database,
    cache: Arc<tokio::sync::RwLock<serde_json::Value>>,
}

impl TaskRepositoryStalledEpicSource {
    pub fn new(db: djinn_db::Database) -> Self {
        Self {
            db,
            cache: Arc::new(tokio::sync::RwLock::new(json!({}))),
        }
    }

    pub async fn refresh(&self) {
        let repo =
            djinn_db::TaskRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());
        let snapshot = match repo.board_health(30).await {
            Ok(report) => report
                .get("stalled_epics")
                .cloned()
                .unwrap_or_else(|| json!({})),
            Err(error) => {
                warn!(%error, "stalled_epic doctor: failed to load snapshot");
                json!({})
            }
        };
        *self.cache.write().await = snapshot;
    }
}

impl StalledEpicSource for TaskRepositoryStalledEpicSource {
    fn snapshot(&self) -> serde_json::Value {
        self.cache
            .try_read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| json!({}))
    }

    fn refresh_for_run(&self) {
        let source = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(source.refresh()));
            }
            Ok(_) => {
                let _ = std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Runtime::new() {
                        runtime.block_on(source.refresh());
                    }
                })
                .join();
            }
            Err(_) => {
                if let Ok(runtime) = tokio::runtime::Runtime::new() {
                    runtime.block_on(source.refresh());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::doctor::DoctorCheck;

    #[test]
    fn reports_exact_1woc_shape_without_offering_a_fix() {
        let source = MemoryStalledEpicSource::new(json!({
            "total": 1,
            "findings": [{
                "id": "epic-1woc", "short_id": "1woc", "title": "Retire verification",
                "tasks": [
                    {"short_id": "w3q8", "issue_type": "planning", "status": "closed"},
                    {"short_id": "vulw", "issue_type": "planning", "status": "closed"},
                    {"short_id": "gy53", "issue_type": "task", "status": "pr_review", "pr_url": "https://example.test/pr/2655"},
                    {"short_id": "zb05", "issue_type": "task", "status": "open", "blocked_by": ["gy53"]}
                ]
            }]
        }));
        let check = StalledEpicCheck::new(Arc::new(source));
        let finding = check.run().unwrap().pop().unwrap();
        assert_eq!(finding.entity_ids["short_id"], "1woc");
        assert_eq!(finding.evidence["epic"]["tasks"][2]["short_id"], "gy53");
        assert!(matches!(
            check.fix(&finding),
            Err(DoctorError::FixNotSupported { .. })
        ));
    }
}
