// CoordinatorActor — 1x global, orchestrates phase execution and task dispatch.
//
// Ryhl hand-rolled actor pattern (AGENT-01):
//   - `CoordinatorHandle` (mpsc sender) is the public API.
//   - `CoordinatorActor` (mpsc receiver) runs in a dedicated tokio task.
//
// Main loop (AGENT-07): tokio::select! over four arms:
//   1. CancellationToken — graceful shutdown.
//   2. mpsc message channel — API calls from MCP tools.
//   3. broadcast::Receiver<DjinnEvent> — react to open-task events.
//   4. 30-second Interval tick — stuck detection safety net (AGENT-08).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{self, Interval};
use tokio_util::sync::CancellationToken;

use crate::actors::git::GitActorHandle;
use crate::actors::slot::{PoolError, SlotPoolHandle};
use crate::agent::AgentType;
use crate::commands::{CommandSpec, run_commands};
use crate::db::connection::Database;
use crate::db::repositories::git_settings::GitSettingsRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::task::{ReadyQuery, TaskRepository};
use crate::events::DjinnEvent;
use crate::provider::catalog::CatalogService;
use crate::provider::health::HealthTracker;

mod dispatch;
mod health;

/// Interval between stuck-detection passes (AGENT-08).
const STUCK_INTERVAL: Duration = Duration::from_secs(30);

/// Cooldown before re-dispatching a task that failed lifecycle setup
/// (e.g. missing credential).  Prevents hot dispatch loops.
const DISPATCH_COOLDOWN: Duration = Duration::from_secs(60);

/// If a task becomes dispatch-ready again within this threshold of its last
/// dispatch, it is considered a rapid failure and placed in cooldown.
const RAPID_FAILURE_THRESHOLD: Duration = Duration::from_secs(10);
#[cfg(test)]
const DEFAULT_MODEL_ID: &str = "test/mock";

// ─── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
}

// ─── Public types ─────────────────────────────────────────────────────────────

/// Snapshot of coordinator runtime state (returned by `CoordinatorHandle::get_status`).
#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub paused: bool,
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Per-project health errors (project_id → error message).
    /// Only populated when queried for a specific project.
    pub unhealthy_projects: HashMap<String, String>,
}

/// Internal snapshot published via `watch` channel so `get_status()` reads
/// never queue behind long-running dispatch passes.
#[derive(Debug, Clone)]
struct SharedCoordinatorState {
    paused_projects: HashSet<String>,
    unhealthy_project_ids: HashSet<String>,
    unhealthy_project_errors: HashMap<String, String>,
    dispatched: u64,
    recovered: u64,
}

impl SharedCoordinatorState {
    fn to_status(&self, project_id: Option<&str>) -> CoordinatorStatus {
        let paused = project_id.is_some_and(|id| {
            self.unhealthy_project_ids.contains(id) || self.paused_projects.contains(id)
        });
        let unhealthy_projects = match project_id {
            Some(id) => self
                .unhealthy_project_errors
                .get(id)
                .map(|err| {
                    let mut m = HashMap::new();
                    m.insert(id.to_string(), err.clone());
                    m
                })
                .unwrap_or_default(),
            None => self.unhealthy_project_errors.clone(),
        };
        CoordinatorStatus {
            paused,
            tasks_dispatched: self.dispatched,
            sessions_recovered: self.recovered,
            unhealthy_projects,
        }
    }
}

// ─── Messages (≤15 variants — AGENT-11) ──────────────────────────────────────

enum CoordinatorMessage {
    /// Run an immediate dispatch pass for all ready tasks.
    TriggerDispatch,
    /// Run an immediate dispatch pass for a specific project.
    TriggerProjectDispatch { project_id: String },
    /// Pause dispatch — no new sessions will start until `Resume`.
    Pause {
        /// If true, interrupt all active sessions immediately.
        interrupt_active: bool,
        /// Optional interruption reason passed to the slot pool.
        reason: String,
    },
    /// Resume dispatch and immediately run a dispatch pass.
    Resume,
    /// Resume dispatch for one project.
    ResumeProject { project_id: String },
    /// Pause dispatch for one project, optionally interrupting active sessions.
    PauseProject {
        project_id: String,
        interrupt_active: bool,
        reason: String,
    },
    /// Update runtime dispatch limit from settings reload.
    UpdateDispatchLimit { limit: usize },
    /// Update per-role model priority list from settings reload.
    UpdateModelPriorities {
        priorities: HashMap<String, Vec<String>>,
    },
    /// Run an immediate stuck-task detection pass.
    TriggerStuckScan,
    /// Trigger background health validation for all (or one) project on execution_start.
    ValidateProjectHealth { project_id_filter: Option<String> },
    /// Internal callback: result from a background project health-check task.
    SetProjectHealth {
        project_id: String,
        healthy: bool,
        error: Option<String>,
    },
}

// ─── Actor (≤20 fields — AGENT-11) ───────────────────────────────────────────

struct CoordinatorActor {
    // Ryhl core
    receiver: mpsc::Receiver<CoordinatorMessage>,
    events: broadcast::Receiver<DjinnEvent>,
    cancel: CancellationToken,
    tick: Interval,
    // Dependencies
    db: Database,
    events_tx: broadcast::Sender<DjinnEvent>,
    pool: SlotPoolHandle,
    #[cfg_attr(test, allow(dead_code))]
    catalog: CatalogService,
    health: HealthTracker,
    // Sender clone for background tasks to send results back.
    self_sender: mpsc::Sender<CoordinatorMessage>,
    // Watch channel for lock-free status reads.
    status_tx: watch::Sender<SharedCoordinatorState>,
    // State
    paused_projects: HashSet<String>,
    dispatch_limit: usize,
    model_priorities: HashMap<String, Vec<String>>,
    // Per-project health: project_id → error message (only unhealthy projects appear here).
    unhealthy_projects: HashMap<String, String>,
    /// Per-task dispatch tracking: task UUID → last dispatch instant.
    /// When a task becomes ready again within `RAPID_FAILURE_THRESHOLD` of its
    /// last dispatch, it is placed in cooldown for `DISPATCH_COOLDOWN` to prevent
    /// hot dispatch loops (e.g. missing credential → release → re-dispatch).
    last_dispatched: HashMap<String, Instant>,
    dispatch_cooldowns: HashMap<String, Instant>,
    // Metrics
    dispatched: u64,
    recovered: u64,
}

// Field count: receiver, events, cancel, tick, db, events_tx, pool,
//              catalog, health, paused, dispatched, recovered = 12 ✓ (≤20)

impl CoordinatorActor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        receiver: mpsc::Receiver<CoordinatorMessage>,
        self_sender: mpsc::Sender<CoordinatorMessage>,
        events_tx: broadcast::Sender<DjinnEvent>,
        cancel: CancellationToken,
        db: Database,
        pool: SlotPoolHandle,
        catalog: CatalogService,
        health: HealthTracker,
        status_tx: watch::Sender<SharedCoordinatorState>,
    ) -> Self {
        let events = events_tx.subscribe();
        let mut tick = time::interval(STUCK_INTERVAL);
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        Self {
            receiver,
            events,
            cancel,
            tick,
            db,
            events_tx,
            pool,
            catalog,
            health,
            self_sender,
            status_tx,
            paused_projects: HashSet::new(),
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            unhealthy_projects: HashMap::new(),
            last_dispatched: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    async fn run(mut self) {
        tracing::info!("CoordinatorActor started");

        // Always start with execution paused for all projects.
        #[cfg(not(test))]
        {
            let repo = crate::db::repositories::project::ProjectRepository::new(
                self.db.clone(),
                self.events_tx.clone(),
            );
            if let Ok(projects) = repo.list().await {
                for p in projects {
                    self.paused_projects.insert(p.id);
                }
            }
            tracing::info!(
                count = self.paused_projects.len(),
                "CoordinatorActor: all projects start paused"
            );
            self.publish_status();
        }

        loop {
            tokio::select! {
                biased;

                // 1. Graceful shutdown via cancellation token.
                _ = self.cancel.cancelled() => {
                    tracing::info!("CoordinatorActor: cancellation token fired, stopping");
                    break;
                }

                // 2. Incoming API messages.
                msg = self.receiver.recv() => {
                    let Some(msg) = msg else {
                        tracing::debug!("CoordinatorActor: message channel closed");
                        break;
                    };
                    self.handle_message(msg).await;
                }

                // 3. Domain events from repositories.
                event = self.events.recv() => {
                    self.handle_event_result(event).await;
                }

                // 4. 30s safety-net tick — stuck detection + dispatch pass for
                //    any tasks that missed an event (e.g. needs_pm_intervention
                //    tasks surviving a server restart).
                _ = self.tick.tick() => {
                    self.detect_and_recover_stuck_filtered(None).await;
                    self.dispatch_ready_tasks(None).await;
                }
            }
        }
        tracing::info!("CoordinatorActor stopped");
    }

    /// Publish current state to the watch channel for lock-free status reads.
    fn publish_status(&self) {
        let _ = self.status_tx.send(SharedCoordinatorState {
            paused_projects: self.paused_projects.clone(),
            unhealthy_project_ids: self.unhealthy_projects.keys().cloned().collect(),
            unhealthy_project_errors: self.unhealthy_projects.clone(),
            dispatched: self.dispatched,
            recovered: self.recovered,
        });
    }

    async fn handle_message(&mut self, msg: CoordinatorMessage) {
        match msg {
            CoordinatorMessage::TriggerDispatch => {
                // Do NOT run stuck detection here — TriggerDispatch fires on
                // every slot-free event (via trigger_redispatch).  Running the
                // stuck detector each time creates a tight loop when
                // prepare_worktree keeps failing: the detector immediately
                // releases the in_progress task back to open, which gets
                // re-dispatched, fails again, frees the slot, triggers
                // dispatch, etc.  The 30-second tick is sufficient for stuck
                // recovery.
                self.dispatch_ready_tasks(None).await;
            }
            CoordinatorMessage::TriggerProjectDispatch { project_id } => {
                self.dispatch_ready_tasks(Some(&project_id)).await;
            }
            CoordinatorMessage::Pause {
                interrupt_active,
                reason,
            } => {
                // Global pause = pause every known project individually.
                let repo = crate::db::repositories::project::ProjectRepository::new(
                    self.db.clone(),
                    self.events_tx.clone(),
                );
                if let Ok(projects) = repo.list().await {
                    for p in projects {
                        self.paused_projects.insert(p.id);
                    }
                }
                tracing::info!(
                    count = self.paused_projects.len(),
                    "CoordinatorActor: paused all projects"
                );
                self.publish_status();
                if interrupt_active && let Err(e) = self.pool.interrupt_all(&reason).await {
                    tracing::warn!(error = %e, "CoordinatorActor: failed to interrupt sessions on pause");
                }
            }
            CoordinatorMessage::PauseProject {
                project_id,
                interrupt_active,
                reason,
            } => {
                self.paused_projects.insert(project_id.clone());
                self.publish_status();
                if interrupt_active
                    && let Err(e) = self.pool.interrupt_project(&project_id, &reason).await
                {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %e,
                        "CoordinatorActor: failed to interrupt project sessions on pause"
                    );
                }
            }
            CoordinatorMessage::Resume => {
                // Global resume = unpause every project and dispatch.
                self.paused_projects.clear();
                tracing::info!("CoordinatorActor: resumed all projects");
                self.detect_and_recover_stuck_filtered(None).await;
                self.dispatch_ready_tasks(None).await;
                self.publish_status();
            }
            CoordinatorMessage::ResumeProject { project_id } => {
                self.paused_projects.remove(&project_id);
                self.detect_and_recover_stuck_filtered(Some(&project_id))
                    .await;
                self.dispatch_ready_tasks(Some(&project_id)).await;
                self.publish_status();
            }
            CoordinatorMessage::TriggerStuckScan => {
                self.detect_and_recover_stuck_filtered(None).await;
            }
            CoordinatorMessage::UpdateDispatchLimit { limit } => {
                let limit = limit.max(1);
                if self.dispatch_limit != limit {
                    tracing::info!(
                        old = self.dispatch_limit,
                        new = limit,
                        "CoordinatorActor: updated dispatch limit"
                    );
                    self.dispatch_limit = limit;
                }
            }
            CoordinatorMessage::UpdateModelPriorities { priorities } => {
                self.model_priorities = priorities;
                tracing::info!("CoordinatorActor: updated per-role model priorities");
            }
            CoordinatorMessage::ValidateProjectHealth { project_id_filter } => {
                self.validate_all_project_health(project_id_filter).await;
            }
            CoordinatorMessage::SetProjectHealth {
                project_id,
                healthy,
                error,
            } => {
                let was_unhealthy = self.unhealthy_projects.contains_key(&project_id);
                if healthy {
                    self.unhealthy_projects.remove(&project_id);
                    tracing::info!(project_id = %project_id, "CoordinatorActor: project health check passed");
                } else {
                    let err = error.clone().unwrap_or_default();
                    tracing::warn!(
                        project_id = %project_id,
                        error = %err,
                        "CoordinatorActor: project health check failed — skipping dispatch for project"
                    );
                    self.unhealthy_projects.insert(project_id.clone(), err);
                }
                // Emit SSE event on health change.
                let changed = healthy && was_unhealthy || !healthy && !was_unhealthy;
                if changed {
                    let _ = self.events_tx.send(DjinnEvent::ProjectHealthChanged {
                        project_id: project_id.clone(),
                        healthy,
                        error,
                    });
                }
                self.publish_status();
                // If project just became healthy and dispatch is enabled, trigger a dispatch pass.
                if healthy && self.is_project_dispatch_enabled(&project_id) {
                    self.dispatch_ready_tasks(Some(&project_id)).await;
                }
            }
        }
    }

    async fn handle_event_result(
        &mut self,
        result: Result<DjinnEvent, broadcast::error::RecvError>,
    ) {
        match result {
            Ok(evt) => self.handle_event(evt).await,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "CoordinatorActor: lagged behind event stream, re-subscribing"
                );
                self.events = self.events_tx.subscribe();
                self.detect_and_recover_stuck_filtered(None).await;
                self.dispatch_ready_tasks(None).await;
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::warn!("CoordinatorActor: event broadcast channel closed");
            }
        }
    }

    async fn handle_event(&mut self, evt: DjinnEvent) {
        match &evt {
            // A task became dispatch-ready for any role → check dispatch.
            // is_project_dispatch_enabled() handles global-pause + per-project
            // resume, so we don't bail early here — a project-resumed project
            // must still react to events even when globally paused.
            // New projects start paused — must be explicitly resumed.
            DjinnEvent::ProjectCreated(p) => {
                self.paused_projects.insert(p.id.clone());
                tracing::info!(
                    project_id = %p.id,
                    "CoordinatorActor: new project starts paused"
                );
                self.publish_status();
            }
            DjinnEvent::TaskCreated { task, .. } | DjinnEvent::TaskUpdated { task, .. }
                if matches!(
                    task.status.as_str(),
                    "open" | "needs_task_review" | "needs_pm_intervention" | "closed"
                ) =>
            {
                tracing::debug!(
                    task_id = %task.short_id,
                    status = %task.status,
                    "CoordinatorActor: ready-task event → dispatch pass"
                );
                self.dispatch_ready_tasks(Some(&task.project_id)).await;
            }
            _ => {}
        }
    }

    fn is_project_dispatch_enabled(&self, project_id: &str) -> bool {
        !self.unhealthy_projects.contains_key(project_id)
            && !self.paused_projects.contains(project_id)
    }

    /// Resolve dispatch models for a given role from configured priorities,
    /// falling back to credential-backed tool-capable models.
    async fn resolve_dispatch_models_for_role(&self, _role: &str) -> Vec<String> {
        #[cfg(test)]
        {
            vec![DEFAULT_MODEL_ID.to_owned()]
        }

        #[cfg(not(test))]
        {
            let cred_repo = crate::db::repositories::credential::CredentialRepository::new(
                self.db.clone(),
                self.events_tx.clone(),
            );
            let credentials = match cred_repo.list().await {
                Ok(credentials) => credentials,
                Err(_) => return Vec::new(),
            };

            let credential_provider_ids = self.catalog.connected_provider_ids(&credentials);
            if credential_provider_ids.is_empty() {
                return Vec::new();
            }

            let mut selected = Vec::new();
            let mut seen = HashSet::new();

            if let Some(priority_models) = self.model_priorities.get(_role) {
                for configured in priority_models {
                    if let Some((provider_id, model_name)) = configured.split_once('/') {
                        if !credential_provider_ids.contains(provider_id) {
                            continue;
                        }
                        // Match by model ID, bare name (after last '/'), display
                        // name, or full configured ID.  Internal IDs may be in
                        // HuggingFace form (e.g. "hf:zai-org/GLM-4.7") while
                        // settings store the API form ("synthetic/GLM-4.7").
                        let exists = self
                            .catalog
                            .list_models(provider_id)
                            .iter()
                            .any(|m| {
                                let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
                                bare == model_name
                                    || m.id == model_name
                                    || m.name == model_name
                                    || m.id == *configured
                            });
                        if exists && seen.insert(configured.clone()) {
                            selected.push(configured.clone());
                        }
                        continue;
                    }

                    if credential_provider_ids.contains(configured) {
                        let models = self.catalog.list_models(configured);
                        if let Some(model) = models.iter().find(|m| m.tool_call) {
                            let model_id = format!("{configured}/{}", model.id);
                            if seen.insert(model_id.clone()) {
                                selected.push(model_id);
                            }
                        }
                        for model in models {
                            let model_id = format!("{configured}/{}", model.id);
                            if seen.insert(model_id.clone()) {
                                selected.push(model_id);
                            }
                        }
                    }
                }
            }

            // Return whatever resolved (may be empty if no priorities configured
            // or all configured providers are disconnected). Never fall back to
            // enumerating random credentials — only dispatch what the user configured.
            selected
        }
    }

    fn role_for_task_status(status: &str) -> &'static str {
        AgentType::for_task_status(status, false).dispatch_role()
    }

    fn task_repo(&self) -> TaskRepository {
        TaskRepository::new(self.db.clone(), self.events_tx.clone())
    }

    async fn project_path_for_id(&self, project_id: &str) -> Option<String> {
        let repo =
            ProjectRepository::new(self.db.clone(), self.events_tx.clone());
        repo.get_path(project_id).await.ok().flatten()
    }
}

// ─── Handle ───────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle to the global `CoordinatorActor`.
#[derive(Clone)]
pub struct CoordinatorHandle {
    sender: mpsc::Sender<CoordinatorMessage>,
    status_rx: watch::Receiver<SharedCoordinatorState>,
}

impl CoordinatorHandle {
    /// Spawn the `CoordinatorActor` and return a handle to it.
    pub fn spawn(
        events_tx: broadcast::Sender<DjinnEvent>,
        cancel: CancellationToken,
        db: Database,
        pool: SlotPoolHandle,
        catalog: CatalogService,
        health: HealthTracker,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let initial_state = SharedCoordinatorState {
            paused_projects: HashSet::new(),
            unhealthy_project_ids: HashSet::new(),
            unhealthy_project_errors: HashMap::new(),
            dispatched: 0,
            recovered: 0,
        };
        let (status_tx, status_rx) = watch::channel(initial_state);
        let actor = CoordinatorActor::new(
            receiver,
            sender.clone(),
            events_tx,
            cancel,
            db,
            pool,
            catalog,
            health,
            status_tx,
        );
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
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
    use crate::db::repositories::epic::EpicRepository;
    use crate::db::repositories::task::TaskRepository;
    use crate::models::task::TransitionAction;
    use crate::provider::health::HealthTracker;
    use crate::server::AppState;
    use crate::test_helpers;

    fn spawn_coordinator(db: &Database, tx: &broadcast::Sender<DjinnEvent>) -> CoordinatorHandle {
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db.clone(), cancel.clone());
        let sessions_dir = std::env::temp_dir().join(format!(
            "djinn-test-sessions-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = sessions_dir;
        let pool = SlotPoolHandle::spawn(
            app_state,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 2,
                    roles: ["worker", "task_reviewer"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let catalog = CatalogService::new();
        let health = HealthTracker::new();
        CoordinatorHandle::spawn(tx.clone(), cancel, db.clone(), pool, catalog, health)
    }

    async fn make_epic(
        db: &Database,
        tx: broadcast::Sender<DjinnEvent>,
    ) -> crate::models::epic::Epic {
        EpicRepository::new(db.clone(), tx)
            .create("Epic", "", "", "", "")
            .await
            .unwrap()
    }

    // ── Status ───────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_status_is_active() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let status = handle.get_status().unwrap();
        assert!(
            !status.paused,
            "coordinator should start active (no global pause state)"
        );
        assert_eq!(status.tasks_dispatched, 0);
        assert_eq!(status.sessions_recovered, 0);
    }

    // ── Pause / Resume ───────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_project_and_resume_toggle_state() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let project_id = "test-project-id";

        // Pausing a project marks it paused in project-scoped status.
        handle.pause_project(project_id).await.unwrap();
        handle
            .wait_for_project_status(project_id, |s| s.paused)
            .await;
        assert!(handle.get_project_status(project_id).unwrap().paused);

        // Resuming removes it from the paused set.
        handle.resume_project(project_id).await.unwrap();
        handle
            .wait_for_project_status(project_id, |s| !s.paused)
            .await;
        assert!(!handle.get_project_status(project_id).unwrap().paused);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_while_project_paused_is_a_noop() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), tx.clone());
        repo.create(&epic.id, "T1", "", "", "task", 0, "")
            .await
            .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.pause_project(&epic.project_id).await.unwrap();
        handle
            .trigger_dispatch_for_project(&epic.project_id)
            .await
            .unwrap();
        // Give the actor a moment to process; dispatched count stays 0.
        tokio::task::yield_now().await;
        assert_eq!(handle.get_status().unwrap().tasks_dispatched, 0);
    }

    // ── Dispatch on open-task event ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_ready_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), tx.clone());

        // Create a ready task (open, no blockers).
        repo.create(&epic.id, "T1", "", "", "task", 0, "")
            .await
            .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.tasks_dispatched >= 1,
            "should have dispatched the ready task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_review_tasks() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), tx.clone());

        let task = repo
            .create(&epic.id, "Review me", "", "", "task", 0, "")
            .await
            .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::Start,
            "test",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "test",
            "system",
            None,
            None,
        )
        .await
        .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        // Dispatch; wait for it to complete.
        handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.tasks_dispatched >= 1,
            "should dispatch task waiting for review"
        );
    }

    // ── Stuck detection ───────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stuck_detection_releases_orphaned_in_progress_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), tx.clone());

        // Manually put a task in_progress (simulating an orphaned session).
        let task = repo
            .create(&epic.id, "Stuck", "", "", "task", 0, "")
            .await
            .unwrap();
        repo.set_status(&task.id, "in_progress").await.unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        // Trigger dispatch to also run stuck detection; wait for recovery.
        handle.wait_for_status(|s| s.sessions_recovered >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.sessions_recovered >= 1,
            "stuck task should have been recovered"
        );

        // The released task should now be back to open.
        let updated = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            updated.status, "open",
            "released task should be back to open"
        );
    }
}
