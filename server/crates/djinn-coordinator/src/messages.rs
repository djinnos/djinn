use std::collections::HashMap;

use crate::supervisor_impl::LiveMoverSummary;

use super::types::{CoordinatorDebugSnapshot, CoordinatorError};

// ─── Messages (≤15 variants — AGENT-11) ──────────────────────────────────────

pub(super) enum CoordinatorMessage {
    /// Run an immediate dispatch pass for all ready tasks.
    TriggerDispatch,
    /// Run an immediate dispatch pass for a specific project.
    TriggerProjectDispatch {
        project_id: String,
    },
    /// Update runtime dispatch limit from settings reload.
    UpdateDispatchLimit {
        limit: usize,
    },
    /// Update per-role model priority list from settings reload.
    UpdateModelPriorities {
        priorities: HashMap<String, Vec<String>>,
    },
    /// Run an immediate stuck-task detection pass.
    TriggerStuckScan,
    // Queue the persisted board-health mismatch scan on the shared coalescer.
    TriggerBoardHealthMismatchScan,
    /// Lead requests Planner escalation for a task.
    /// Creates a review task and dispatches Planner to it.
    /// Per ADR-051 §8 the Planner is the escalation ceiling above Lead.
    DispatchPlannerEscalation {
        source_task_id: String,
        reason: String,
        project_id: String,
    },
    /// Route a completed loop-guard trip through the shared Planner-intervention
    /// machinery instead of the dispatch-failure streak/ladder.
    RouteLoopGuardPlannerIntervention {
        source_task_id: String,
        role: &'static str,
        reason: String,
    },
    /// Clear same-role dispatch-failure state after a planned terminal lifecycle
    /// ending (for example a budget park). The next continuation dispatch must
    /// start from the disposition ladder, not from stale failure accounting.
    ClearPlannedDispatchCompletion {
        task_id: String,
        reason: String,
    },
    /// Collect coordinator/repository evidence for the task and return the
    /// reusable live-mover summary exposed outside the PR-open path.
    #[allow(dead_code)]
    CheckLiveMover {
        task_id: String,
        reply: tokio::sync::oneshot::Sender<Result<LiveMoverSummary, CoordinatorError>>,
    },
    /// Route a settled task that has no live mover through the shared no-op
    /// disposition ladder (if the live-mover predicate says it is orphaned).
    RouteSettledNoopWithoutLiveMover {
        task_id: String,
    },
    /// Publish scrape-time coordinator live-state gauges without exposing
    /// per-task labels or storing per-task metric state.
    RecordLiveMetrics {
        reply: tokio::sync::oneshot::Sender<()>,
    },
    /// Return a read-only debug snapshot of coordinator dispatch state.
    DebugSnapshot {
        reply: tokio::sync::oneshot::Sender<CoordinatorDebugSnapshot>,
    },
    /// Post-commit durable refinement wake. The actor reloads this exact run
    /// before rebuilding a disposable projection.
    WakeRefinementRun {
        run_id: String,
    },
    /// Legacy compatibility surface; durable admission is not actor authority.
    StartProposalRefinement {
        proposal_id: String,
        current_revision_seq: i32,
        /// User the refinement run is attributed to (task owner + model scope).
        /// `None` falls back to the proposal author at dispatch time.
        owner_user_id: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Demand another tribunal round for a proposal whose refinement has
    /// stopped. The coordinator clears the stop state and re-enqueues the
    /// refinement loop for another round. Returns an error if refinement
    /// is still active (already running).
    DemandProposalRefinementRound {
        proposal_id: String,
        current_revision_seq: i32,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Resolve the human's single accept/reject review of a converged
    /// refinement. `accept` keeps the refined spec; reject reverts to the
    /// pre-refinement snapshot. `feedback` is recorded for the audit trail.
    ResolveRefinementReview {
        proposal_id: String,
        accept: bool,
        feedback: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}
