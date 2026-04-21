use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::actor::CoordinatorActor;
use super::consolidation::DbConsolidationRunner;
use super::messages::CoordinatorMessage;
use super::types::*;

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
        let _ = self.sender.try_send(CoordinatorMessage::TriggerDispatch);
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
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
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

    /// Trigger an immediate Planner patrol dispatch (for testing).
    /// Per ADR-051 §1 the Planner owns the board patrol.
    #[cfg(test)]
    pub async fn trigger_planner_patrol(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerPlannerPatrol).await
    }

    /// Dispatch a Planner escalation for a task.
    ///
    /// Creates a review task and dispatches the Planner to it.
    /// Called when Lead uses `request_planner` or auto-escalation fires on 2nd `request_lead`.
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

    /// Increment the Lead escalation count for a task and return the new count.
    ///
    /// When the count reaches ≥ 2, the caller should route to Planner instead of Lead
    /// (per ADR-051 §8).
    pub async fn increment_escalation_count(&self, task_id: &str) -> Result<u32, CoordinatorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(CoordinatorMessage::IncrementEscalationCount {
                task_id: task_id.to_owned(),
                reply: tx,
            })
            .await
            .map_err(|_| CoordinatorError::ActorDead)?;
        rx.await.map_err(|_| CoordinatorError::NoResponse)
    }
}
