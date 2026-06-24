use std::collections::HashMap;

use async_trait::async_trait;

/// Parameters for starting a proposal refinement run via the coordinator.
#[derive(Debug, Clone)]
pub struct ProposalRefinementStartRequest {
    /// The proposal UUID to refine.
    pub proposal_id: String,
    /// The current `latest_revision_seq` of the proposal at the time the
    /// refinement was requested. The coordinator uses this to initialise
    /// `RefinementLoopState::current_revision_seq`.
    pub current_revision_seq: i32,
    /// Update authority mode: `"checkpoint"` or `"auto_accept"`.
    pub update_authority: String,
}

#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Tasks merged per hour per epic in the past hour (epic_id → count).
    pub epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (e.g. org OAuth restrictions).
    pub pr_errors: HashMap<String, String>,
}

// ── Coordinator ─────────────────────────────────────────────────────────────────

#[async_trait]
pub trait CoordinatorOps: Send + Sync {
    fn get_status(&self) -> Result<CoordinatorStatus, String>;
    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String>;
    /// Start a proposal refinement run.  The coordinator is authoritative for
    /// duplicate-start rejection — if the proposal already has an active
    /// refinement loop the call returns an error.
    async fn start_proposal_refinement(
        &self,
        request: ProposalRefinementStartRequest,
    ) -> Result<(), String>;
}
