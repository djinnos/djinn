use std::collections::{HashMap, HashSet};
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
            paused_projects: HashSet::new(),
            unhealthy_project_ids: HashSet::new(),
            unhealthy_project_errors: HashMap::new(),
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
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

    /// Pause dispatch (no new sessions will start).
    pub async fn pause(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::Pause {
            interrupt_active: false,
            reason: "session interrupted by coordinator pause".to_string(),
        })
        .await
    }

    pub async fn pause_project(&self, project_id: &str) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::PauseProject {
            project_id: project_id.to_owned(),
            interrupt_active: false,
            reason: String::new(),
        })
        .await
    }

    pub async fn pause_project_immediate(
        &self,
        project_id: &str,
        reason: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::PauseProject {
            project_id: project_id.to_owned(),
            interrupt_active: true,
            reason: reason.to_owned(),
        })
        .await
    }

    /// Pause dispatch and interrupt active sessions immediately.
    pub async fn pause_immediate(&self, reason: &str) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::Pause {
            interrupt_active: true,
            reason: reason.to_owned(),
        })
        .await
    }

    /// Resume dispatch and immediately run a dispatch pass.
    pub async fn resume(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::Resume).await
    }

    pub async fn resume_project(&self, project_id: &str) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::ResumeProject {
            project_id: project_id.to_owned(),
        })
        .await
    }

    /// Return the current coordinator status snapshot (lock-free read via watch channel).
    pub fn get_status(&self) -> Result<CoordinatorStatus, CoordinatorError> {
        Ok(self.status_rx.borrow().to_status(None))
    }

    pub fn get_project_status(
        &self,
        project_id: &str,
    ) -> Result<CoordinatorStatus, CoordinatorError> {
        Ok(self.status_rx.borrow().to_status(Some(project_id)))
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

    /// Trigger background project health validation on execution_start (ADR-014).
    /// Scoped to `project_id_filter` if provided, otherwise validates all projects.
    pub async fn validate_project_health(
        &self,
        project_id_filter: Option<String>,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::ValidateProjectHealth { project_id_filter })
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
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if predicate(&rx.borrow().to_status(None)) {
                return;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => panic!("watch channel closed"),
                Err(_) => panic!("timed out waiting for coordinator status condition"),
            }
        }
    }

    /// Like `wait_for_status` but evaluates the predicate against project-scoped status.
    #[cfg(test)]
    pub async fn wait_for_project_status<F>(&self, project_id: &str, predicate: F)
    where
        F: Fn(&CoordinatorStatus) -> bool,
    {
        let project_id = project_id.to_owned();
        let mut rx = self.status_rx.clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if predicate(&rx.borrow().to_status(Some(&project_id))) {
                return;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => panic!("watch channel closed"),
                Err(_) => panic!("timed out waiting for coordinator project status condition"),
            }
        }
    }

    /// Trigger an immediate Architect patrol dispatch (for testing).
    #[cfg(test)]
    pub async fn trigger_architect_patrol(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::TriggerArchitectPatrol).await
    }

    /// Dispatch an Architect escalation for a task.
    ///
    /// Creates a review task and dispatches the Architect to it.
    /// Called when Lead uses `request_architect` or auto-escalation fires on 2nd `request_lead`.
    pub async fn dispatch_architect_escalation(
        &self,
        source_task_id: &str,
        reason: &str,
        project_id: &str,
    ) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::DispatchArchitectEscalation {
            source_task_id: source_task_id.to_owned(),
            reason: reason.to_owned(),
            project_id: project_id.to_owned(),
        })
        .await
    }

    /// Increment the Lead escalation count for a task and return the new count.
    ///
    /// When the count reaches ≥ 2, the caller should route to Architect instead of Lead.
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
