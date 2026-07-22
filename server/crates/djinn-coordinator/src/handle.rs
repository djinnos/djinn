use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use djinn_core::clock::{Clock, SystemClock};
use tokio::sync::{
    mpsc::{self, error::TrySendError},
    watch,
};

use super::actor::CoordinatorActor;
use super::consolidation::DbConsolidationRunner;
use super::messages::CoordinatorMessage;
use super::types::*;
use crate::supervisor_impl::LiveMoverSummary;
use djinn_orchestration_types::trigger::CoordinatorTrigger;

const TRY_TRIGGER_DISPATCH_LOG_INTERVAL_SECS: u64 = 30;

static LAST_FULL_TRY_TRIGGER_DISPATCH_LOG_SECS: AtomicU64 = AtomicU64::new(0);
static LAST_CLOSED_TRY_TRIGGER_DISPATCH_LOG_SECS: AtomicU64 = AtomicU64::new(0);

// ─── Handle ───────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle to the global `CoordinatorActor`.
#[derive(Clone)]
pub struct CoordinatorHandle {
    sender: mpsc::Sender<CoordinatorMessage>,
    status_rx: watch::Receiver<SharedCoordinatorState>,
}

impl CoordinatorHandle {
    /// Spawn the `CoordinatorActor` and return a handle to it.
    pub fn spawn(deps: CoordinatorDeps) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let initial_state = SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        };
        let (status_tx, status_rx) = watch::channel(initial_state);
        let deps = CoordinatorDeps {
            consolidation_runner: Some(
                deps.consolidation_runner
                    .unwrap_or_else(|| Arc::new(DbConsolidationRunner::new(deps.db.clone()))),
            ),
            ..deps
        };
        let actor = CoordinatorActor::new(deps, receiver, sender.clone(), status_tx);
        tokio::spawn(actor.run());
        Self { sender, status_rx }
    }

    async fn send(&self, msg: CoordinatorMessage) -> Result<(), CoordinatorError> {
        self.sender
            .send(msg)
            .await
            .map_err(|_| CoordinatorError::ActorDead)
    }

    /// Trigger an immediate dispatch pass for all ready tasks.
    pub async fn trigger_dispatch(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerDispatch).await
    }

    /// Best-effort dispatch trigger that never blocks.
    ///
    /// Used by the pool actor's slot-completion handler to avoid a deadlock:
    /// the pool must not `.await` on the coordinator channel while the
    /// coordinator may be `.await`-ing on the pool (e.g. `has_session`).
    pub fn try_trigger_dispatch(&self) {
        match self.sender.try_send(CoordinatorMessage::TriggerDispatch) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if should_log_try_trigger_dispatch_failure(&LAST_FULL_TRY_TRIGGER_DISPATCH_LOG_SECS)
                {
                    tracing::warn!(
                        "coordinator dispatch trigger skipped: coordinator channel is busy/backpressured"
                    );
                }
            }
            Err(TrySendError::Closed(_)) => {
                if should_log_try_trigger_dispatch_failure(
                    &LAST_CLOSED_TRY_TRIGGER_DISPATCH_LOG_SECS,
                ) {
                    tracing::error!(
                        "coordinator dispatch trigger failed: coordinator channel is closed/dead"
                    );
                }
            }
        }
    }

    pub async fn trigger_dispatch_for_project(
        &self,
        project_id: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerProjectDispatch {
            project_id: project_id.to_owned(),
        })
        .await
    }

    /// Return the current coordinator status snapshot (lock-free read via watch channel).
    pub fn get_status(&self) -> Result<CoordinatorStatus, CoordinatorError> {
        Ok(self.status_rx.borrow().to_status())
    }

    /// Trigger an immediate stuck-task detection pass.
    pub async fn trigger_stuck_scan(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerStuckScan).await
    }

    pub async fn trigger_board_health_mismatch_scan(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerBoardHealthMismatchScan)
            .await
    }

    /// Update ready-task dispatch limit.
    pub async fn update_dispatch_limit(&self, limit: usize) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::UpdateDispatchLimit {
            limit: limit.max(1),
        })
        .await
    }

    /// Update per-role model priority lists.
    pub async fn update_model_priorities(
        &self,
        priorities: HashMap<String, Vec<String>>,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::UpdateModelPriorities { priorities })
            .await
    }

    /// Wait until the coordinator status satisfies the given predicate.
    /// For use in tests where we need to observe the effect of a sent message.
    #[cfg(test)]
    pub async fn wait_for_status<F>(&self, predicate: F)
    where
        F: Fn(&CoordinatorStatus) -> bool,
    {
        let mut rx = self.status_rx.clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            if predicate(&rx.borrow().to_status()) {
                return;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => panic!("watch channel closed"),
                Err(_) => panic!("timed out waiting for coordinator status condition"),
            }
        }
    }

    /// Dispatch a Planner escalation for a task.
    ///
    /// Creates a review task and dispatches the Planner to it.
    /// Called when Lead uses `request_planner` or auto-escalation fires on 2nd planner escalation.
    /// Per ADR-051 §8 the Planner is the escalation ceiling above Lead.
    pub async fn dispatch_planner_escalation(
        &self,
        source_task_id: &str,
        reason: &str,
        project_id: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::DispatchPlannerEscalation {
            source_task_id: source_task_id.to_owned(),
            reason: reason.to_owned(),
            project_id: project_id.to_owned(),
        })
        .await
    }

    /// Route a loop-guard-terminated task through the shared Planner
    /// intervention / second-strike park path. This is intentionally distinct
    /// from the ordinary dispatch-failure redispatch ladder.
    pub async fn route_loop_guard_planner_intervention(
        &self,
        source_task_id: &str,
        role: &'static str,
        reason: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::RouteLoopGuardPlannerIntervention {
            source_task_id: source_task_id.to_owned(),
            role,
            reason: reason.to_owned(),
        })
        .await
    }

    /// Clear stale dispatch-failure/cooldown markers for a task whose previous
    /// run ended deliberately (for example a budget park) rather than as a
    /// dispatch/provider fault.
    pub async fn clear_planned_dispatch_completion(
        &self,
        task_id: &str,
        reason: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::ClearPlannedDispatchCompletion {
            task_id: task_id.to_owned(),
            reason: reason.to_owned(),
        })
        .await
    }

    /// Return the reusable non-PR live-mover summary for a task.
    ///
    /// This is the coordinator/API call surface for post-run and orphan-task
    /// checks: callers ask the coordinator to collect hard runtime/board
    /// evidence, then receive the pure supervisor-side summary without importing
    /// PR-open disposition code.
    #[allow(dead_code)]
    pub(crate) async fn live_mover_summary(
        &self,
        task_id: &str,
    ) -> Result<LiveMoverSummary, CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::CheckLiveMover {
                task_id: task_id.to_owned(),
                reply: tx,
            })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await.map_err(|_| CoordinatorError::NoResponse)?
    }

    /// Route a settled no-op task through the shared no-mover disposition path
    /// if the coordinator's live-mover predicate finds no active owner.
    pub async fn route_settled_noop_without_live_mover(
        &self,
        task_id: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::RouteSettledNoopWithoutLiveMover {
            task_id: task_id.to_owned(),
        })
        .await
    }

    /// Ask the coordinator actor to publish its scrape-time live-state gauges.
    ///
    /// The actor owns the in-flight ledger and cooldown maps, so this request
    /// snapshots their aggregate sizes on the actor thread and emits only scalar
    /// gauges. No per-task labels are created and no caller holds an application
    /// lock while awaiting the actor response.
    pub async fn record_live_metrics(&self) -> Result<(), CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::RecordLiveMetrics { reply: tx })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await.map_err(|_| CoordinatorError::NoResponse)
    }

    /// Return a read-only debug snapshot of coordinator dispatch state.
    pub async fn debug_dispatch_state(&self) -> Result<CoordinatorDebugSnapshot, CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::DebugSnapshot { reply: tx })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await.map_err(|_| CoordinatorError::NoResponse)
    }

    /// Deliver a post-commit refinement wake keyed by the exact durable run.
    pub async fn wake_refinement_run(&self, run_id: String) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::WakeRefinementRun { run_id }).await
    }

    /// Legacy compatibility surface; durable admission is outside this actor.
    pub async fn start_proposal_refinement(
        &self,
        proposal_id: String,
        current_revision_seq: i32,
        owner_user_id: Option<String>,
    ) -> Result<(), CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::StartProposalRefinement {
                proposal_id,
                current_revision_seq,
                owner_user_id,
                reply: tx,
            })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await
            .map_err(|_| CoordinatorError::NoResponse)?
            .map_err(CoordinatorError::Other)
    }

    /// Demand another tribunal round for a proposal whose refinement has
    /// stopped. The coordinator clears the stop state and re-enqueues
    /// the refinement loop for another Advocate→Adversary→Judge cycle.
    pub async fn demand_proposal_refinement_round(
        &self,
        proposal_id: String,
        current_revision_seq: i32,
    ) -> Result<(), CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::DemandProposalRefinementRound {
                proposal_id,
                current_revision_seq,
                reply: tx,
            })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await
            .map_err(|_| CoordinatorError::NoResponse)?
            .map_err(CoordinatorError::Other)
    }

    /// Resolve the human's accept/reject review of a converged refinement.
    pub async fn resolve_refinement_review(
        &self,
        proposal_id: String,
        accept: bool,
        feedback: Option<String>,
    ) -> Result<(), CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::ResolveRefinementReview {
                proposal_id,
                accept,
                feedback,
                reply: tx,
            })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await
            .map_err(|_| CoordinatorError::NoResponse)?
            .map_err(CoordinatorError::Other)
    }
}

// ─── CoordinatorTrigger impl ───────────────────────────────────────────────

impl CoordinatorTrigger for CoordinatorHandle {
    fn try_trigger_dispatch(&self) {
        self.try_trigger_dispatch();
    }

    fn try_route_no_progress_intervention(&self, task_id: &str, reason: &str) {
        match self
            .sender
            .try_send(CoordinatorMessage::RouteLoopGuardPlannerIntervention {
                source_task_id: task_id.to_owned(),
                role: "worker",
                reason: reason.to_owned(),
            }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    task_id,
                    "coordinator no_progress_submission intervention skipped: channel busy"
                );
            }
            Err(TrySendError::Closed(_)) => {
                tracing::error!(
                    task_id,
                    "coordinator no_progress_submission intervention failed: channel closed"
                );
            }
        }
    }
}

fn should_log_try_trigger_dispatch_failure(last_log_secs: &AtomicU64) -> bool {
    let now_secs = SystemClock::new()
        .now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let last_secs = last_log_secs.load(Ordering::Relaxed);
    if now_secs.saturating_sub(last_secs) < TRY_TRIGGER_DISPATCH_LOG_INTERVAL_SECS {
        return false;
    }

    last_log_secs
        .compare_exchange(last_secs, now_secs, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}
