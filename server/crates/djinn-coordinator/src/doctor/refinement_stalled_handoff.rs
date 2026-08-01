//! Read-only detector for a terminal role task whose materialized intent has
//! not been consumed. This intentionally does not consult liveness or age.

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};
use serde_json::json;
use std::sync::Arc;

pub const REFINEMENT_STALLED_HANDOFF_CHECK_NAME: &str = "refinement_stalled_handoff";

pub trait RefinementStalledHandoffSource: Send + Sync {
    fn findings(&self) -> Vec<djinn_db::RefinementStalledHandoff>;
    fn refresh_for_run(&self) {}
}

pub struct RefinementStalledHandoffCheck {
    source: Arc<dyn RefinementStalledHandoffSource>,
}
impl RefinementStalledHandoffCheck {
    pub fn new(source: Arc<dyn RefinementStalledHandoffSource>) -> Self {
        Self { source }
    }
}
impl DoctorCheck for RefinementStalledHandoffCheck {
    fn name(&self) -> &'static str {
        REFINEMENT_STALLED_HANDOFF_CHECK_NAME
    }
    fn description(&self) -> &'static str {
        "Reports materialized refinement intents with terminal role tasks and no successor; read-only"
    }
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }
    fn run(&self) -> DoctorResult<Vec<Finding>> {
        self.source.refresh_for_run();
        let mut rows = self.source.findings();
        rows.sort_by(|a, b| {
            (&a.proposal_id, &a.run_id, &a.intent_id).cmp(&(
                &b.proposal_id,
                &b.run_id,
                &b.intent_id,
            ))
        });
        Ok(rows.into_iter().map(|row| {
            let inputs = json!({"proposal_id":row.proposal_id,"run_id":row.run_id,"generation":row.generation,"intent_id":row.intent_id,"task_id":row.task_id});
            Finding::new(FindingSeverity::Warn, REFINEMENT_STALLED_HANDOFF_CHECK_NAME,
                ResolverSnapshot::new("terminal_task_without_successor", inputs.clone(), json!({"stalled":true})),
                format!("refinement run {} has terminal role task {} without a recorded outcome", row.run_id, row.task_id))
                .with_entity_id("proposal_id", row.proposal_id.clone())
                .with_entity_id("run_id", row.run_id.clone())
                .with_entity_id("task_id", row.task_id.clone())
                .with_evidence(json!({"proposal_id":row.proposal_id,"run_id":row.run_id,"generation":row.generation,"intent_id":row.intent_id,"role_task_id":row.task_id,"task_status":row.task_status,"task_terminal_at":row.task_terminal_at,"terminal_elapsed_seconds":row.terminal_elapsed_seconds,"outcome_attempts":row.outcome_attempts}))
        }).collect())
    }
}

#[derive(Clone)]
pub struct ProposalRepositoryRefinementStalledHandoffSource {
    db: djinn_db::Database,
    events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    cache: Arc<tokio::sync::RwLock<Vec<djinn_db::RefinementStalledHandoff>>>,
}
impl ProposalRepositoryRefinementStalledHandoffSource {
    pub fn new(
        db: djinn_db::Database,
        events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    ) -> Self {
        Self {
            db,
            events_tx,
            cache: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }
    async fn refresh(&self) {
        let repo = djinn_db::ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        match repo.load_refinement_stalled_handoffs().await {
            Ok(rows) => *self.cache.write().await = rows,
            Err(error) => {
                tracing::warn!(%error, "refinement_stalled_handoff doctor refresh failed")
            }
        }
    }
    fn refresh_blocking(&self) {
        let source = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(source.refresh()))
            }
            _ => {
                let _ = std::thread::spawn(move || {
                    if let Ok(rt) = tokio::runtime::Runtime::new() {
                        rt.block_on(source.refresh())
                    }
                })
                .join();
            }
        }
    }
}
impl RefinementStalledHandoffSource for ProposalRepositoryRefinementStalledHandoffSource {
    fn findings(&self) -> Vec<djinn_db::RefinementStalledHandoff> {
        self.cache.try_read().map(|v| v.clone()).unwrap_or_default()
    }
    fn refresh_for_run(&self) {
        self.refresh_blocking();
    }
}
