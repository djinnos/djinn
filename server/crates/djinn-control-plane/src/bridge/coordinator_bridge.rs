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
    /// User the refinement run is attributed to: owner of the spawned
    /// refinement tasks and the scope for per-user role-model resolution.
    /// `None` falls back to the proposal author at dispatch time.
    pub owner_user_id: Option<String>,
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
    /// Demand another tribunal round for a proposal whose refinement has
    /// stopped (e.g. after a judge verdict or round-cap). The coordinator
    /// clears the stop state and re-enqueues the refinement loop for
    /// another Advocate→Adversary→Judge cycle. Returns an error if
    /// refinement is still active (already running).
    async fn demand_proposal_refinement_round(
        &self,
        request: ProposalRefinementStartRequest,
    ) -> Result<(), String>;
}
