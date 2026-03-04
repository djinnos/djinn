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
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{self, Interval};
use tokio_util::sync::CancellationToken;

use crate::actors::git::GitActorHandle;
use crate::actors::supervisor::{AgentSupervisorHandle, SupervisorError};
use crate::commands::{CommandSpec, run_commands};
use crate::db::connection::Database;
use crate::db::repositories::git_settings::GitSettingsRepository;
use crate::db::repositories::task::{ReadyQuery, TaskRepository};
use crate::events::DjinnEvent;
use crate::models::task::TransitionAction;
use crate::provider::catalog::CatalogService;
use crate::provider::health::HealthTracker;

/// Interval between stuck-detection passes (AGENT-08).
const STUCK_INTERVAL: Duration = Duration::from_secs(30);
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
    pub global_paused: bool,
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
}

// ─── Messages (≤15 variants — AGENT-11) ──────────────────────────────────────

type Reply<T> = oneshot::Sender<T>;

enum CoordinatorMessage {
    /// Run an immediate dispatch pass for all ready tasks.
    TriggerDispatch,
    /// Run an immediate dispatch pass for a specific project.
    TriggerProjectDispatch { project_id: String },
    /// Pause dispatch — no new sessions will start until `Resume`.
    Pause {
        /// If true, interrupt all active sessions immediately.
        interrupt_active: bool,
        /// Optional interruption reason passed to the supervisor.
        reason: String,
    },
    /// Resume dispatch and immediately run a dispatch pass.
    Resume,
    /// Resume dispatch for one project.
    ResumeProject { project_id: String },
    /// Return current coordinator status.
    GetStatus {
        project_id: Option<String>,
        respond_to: Reply<CoordinatorStatus>,
    },
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
    supervisor: AgentSupervisorHandle,
    catalog: CatalogService,
    health: HealthTracker,
    // Sender clone for background tasks to send results back.
    self_sender: mpsc::Sender<CoordinatorMessage>,
    // State
    paused: bool,
    paused_projects: HashSet<String>,
    resumed_projects: HashSet<String>,
    dispatch_limit: usize,
    model_priorities: HashMap<String, Vec<String>>,
    // Per-project health: project_id → error message (only unhealthy projects appear here).
    unhealthy_projects: HashMap<String, String>,
    // Metrics
    dispatched: u64,
    recovered: u64,
}

// Field count: receiver, events, cancel, tick, db, events_tx, supervisor,
//              catalog, health, paused, dispatched, recovered = 12 ✓ (≤20)

impl CoordinatorActor {
    fn new(
        receiver: mpsc::Receiver<CoordinatorMessage>,
        self_sender: mpsc::Sender<CoordinatorMessage>,
        events_tx: broadcast::Sender<DjinnEvent>,
        cancel: CancellationToken,
        db: Database,
        supervisor: AgentSupervisorHandle,
        catalog: CatalogService,
        health: HealthTracker,
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
            supervisor,
            catalog,
            health,
            self_sender,
            paused: true,
            paused_projects: HashSet::new(),
            resumed_projects: HashSet::new(),
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            unhealthy_projects: HashMap::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    async fn run(mut self) {
        tracing::info!("CoordinatorActor started");
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

                // 4. 30s safety-net tick for stuck detection (AGENT-08).
                _ = self.tick.tick() => {
                    if !self.paused {
                        self.detect_and_recover_stuck_filtered(None).await;
                    }
                }
            }
        }
        tracing::info!("CoordinatorActor stopped");
    }

    async fn handle_message(&mut self, msg: CoordinatorMessage) {
        match msg {
            CoordinatorMessage::TriggerDispatch => {
                if !self.paused {
                    self.detect_and_recover_stuck_filtered(None).await;
                    self.dispatch_ready_tasks(None).await;
                }
            }
            CoordinatorMessage::TriggerProjectDispatch { project_id } => {
                self.detect_and_recover_stuck_filtered(Some(&project_id))
                    .await;
                self.dispatch_ready_tasks(Some(&project_id)).await;
            }
            CoordinatorMessage::Pause {
                interrupt_active,
                reason,
            } => {
                if !self.paused {
                    tracing::info!("CoordinatorActor: paused");
                    self.paused = true;
                    self.paused_projects.clear();
                    self.resumed_projects.clear();
                    if interrupt_active && let Err(e) = self.supervisor.interrupt_all(&reason).await
                    {
                        tracing::warn!(error = %e, "CoordinatorActor: failed to interrupt active sessions on pause");
                    }
                }
            }
            CoordinatorMessage::PauseProject {
                project_id,
                interrupt_active,
                reason,
            } => {
                if self.paused {
                    self.resumed_projects.remove(&project_id);
                } else {
                    self.paused_projects.insert(project_id.clone());
                }
                if interrupt_active {
                    if let Err(e) = self.supervisor.interrupt_project(&project_id, &reason).await {
                        tracing::warn!(
                            project_id = %project_id,
                            error = %e,
                            "CoordinatorActor: failed to interrupt project sessions on pause"
                        );
                    }
                }
            }
            CoordinatorMessage::Resume => {
                if self.paused {
                    tracing::info!("CoordinatorActor: resumed");
                    self.paused = false;
                    self.paused_projects.clear();
                    self.resumed_projects.clear();
                    self.detect_and_recover_stuck_filtered(None).await;
                    self.dispatch_ready_tasks(None).await;
                }
            }
            CoordinatorMessage::ResumeProject { project_id } => {
                if self.paused {
                    self.resumed_projects.insert(project_id.clone());
                } else {
                    self.paused_projects.remove(&project_id);
                }
                self.detect_and_recover_stuck_filtered(Some(&project_id))
                    .await;
                self.dispatch_ready_tasks(Some(&project_id)).await;
            }
            CoordinatorMessage::GetStatus {
                project_id,
                respond_to,
            } => {
                let paused = project_id
                    .as_deref()
                    .map(|id| !self.is_project_dispatch_enabled(id))
                    .unwrap_or(self.paused);
                let _ = respond_to.send(CoordinatorStatus {
                    paused,
                    global_paused: self.paused,
                    tasks_dispatched: self.dispatched,
                    sessions_recovered: self.recovered,
                });
            }
            CoordinatorMessage::TriggerStuckScan => {
                if !self.paused {
                    self.detect_and_recover_stuck_filtered(None).await;
                }
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
                if !self.paused {
                    self.detect_and_recover_stuck_filtered(None).await;
                    self.dispatch_ready_tasks(None).await;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::warn!("CoordinatorActor: event broadcast channel closed");
            }
        }
    }

    async fn handle_event(&mut self, evt: DjinnEvent) {
        if self.paused {
            return;
        }
        match &evt {
            // A task became dispatch-ready for any role → check dispatch.
            DjinnEvent::TaskCreated(t) | DjinnEvent::TaskUpdated(t)
                if matches!(t.status.as_str(), "open" | "needs_task_review" | "closed") =>
            {
                tracing::debug!(
                    task_id = %t.short_id,
                    status = %t.status,
                    "CoordinatorActor: ready-task event → dispatch pass"
                );
                self.dispatch_ready_tasks(Some(&t.project_id)).await;
            }
            _ => {}
        }
    }

    fn is_project_dispatch_enabled(&self, project_id: &str) -> bool {
        if self.unhealthy_projects.contains_key(project_id) {
            return false;
        }
        if self.paused {
            self.resumed_projects.contains(project_id)
        } else {
            !self.paused_projects.contains(project_id)
        }
    }

    /// Resolve dispatch models for a given role from configured priorities,
    /// falling back to credential-backed tool-capable models.
    async fn resolve_dispatch_models_for_role(&self, _role: &str) -> Vec<String> {
        #[cfg(test)]
        {
            return vec![DEFAULT_MODEL_ID.to_owned()];
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
            if credentials.is_empty() {
                return Vec::new();
            }

            let mut credential_provider_ids: HashSet<String> =
                credentials.iter().map(|c| c.provider_id.clone()).collect();

            // Also consider OAuth-connected providers (e.g. chatgpt_codex).
            let goose_entries =
                crate::mcp::tools::provider_tools::goose_provider_entries().await;
            for provider in self.catalog.list_providers() {
                let oauth_keys =
                    crate::mcp::tools::provider_tools::oauth_keys_for_provider(
                        &provider.id,
                        &goose_entries,
                    );
                if !oauth_keys.is_empty()
                    && crate::mcp::tools::provider_tools::is_oauth_key_present(&oauth_keys)
                {
                    credential_provider_ids.insert(provider.id.clone());
                }
            }

            let mut selected = Vec::new();
            let mut seen = HashSet::new();

            if let Some(priority_models) = self.model_priorities.get(_role) {
                for configured in priority_models {
                    if let Some((provider_id, model_name)) = configured.split_once('/') {
                        if !credential_provider_ids.contains(provider_id) {
                            continue;
                        }
                        let exists = self
                            .catalog
                            .list_models(provider_id)
                            .iter()
                            .any(|m| m.id == model_name);
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

            if !selected.is_empty() {
                return selected;
            }

            // When model_priorities are configured but none resolved (e.g. provider
            // disconnected), do NOT fall back to enumerating every credential —
            // that spawns sessions for random models the user never configured.
            if !self.model_priorities.is_empty() {
                return selected; // empty — dispatch_ready_tasks will skip
            }

            // Only enumerate all providers when NO priorities are configured at all
            // (backwards compat for unconfigured setups).
            for cred in &credentials {
                let models = self.catalog.list_models(&cred.provider_id);
                // Prefer a model with tool_call capability.
                if let Some(model) = models.iter().find(|m| m.tool_call) {
                    let model_id = format!("{}/{}", cred.provider_id, model.id);
                    if seen.insert(model_id.clone()) {
                        selected.push(model_id);
                    }
                }

                for model in models {
                    let model_id = format!("{}/{}", cred.provider_id, model.id);
                    if seen.insert(model_id.clone()) {
                        selected.push(model_id);
                    }
                }
            }

            selected
        }
    }

    fn role_for_task_status(status: &str) -> &'static str {
        match status {
            "needs_task_review" | "in_task_review" => "task_reviewer",
            _ => "worker",
        }
    }

    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    async fn dispatch_ready_tasks(&mut self, project_filter: Option<&str>) {
        let mut role_models: HashMap<&'static str, Vec<String>> = HashMap::new();
        for role in ["worker", "task_reviewer", "epic_reviewer"] {
            let model_ids = self.resolve_dispatch_models_for_role(role).await;
            if !model_ids.is_empty() {
                role_models.insert(role, model_ids);
            }
        }
        if role_models.is_empty() {
            tracing::debug!("CoordinatorActor: no configured model found, skipping dispatch");
            return;
        }

        let repo = self.task_repo();
        let mut ready = match repo
            .list_ready(ReadyQuery {
                issue_type: None,
                limit: self.dispatch_limit as i64,
                ..Default::default()
            })
            .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_ready failed");
                return;
            }
        };

        for status in ["needs_task_review"] {
            match repo.list_by_status(status).await {
                Ok(mut tasks) => ready.append(&mut tasks),
                Err(e) => {
                    tracing::warn!(error = %e, status, "CoordinatorActor: list_by_status failed");
                }
            }
        }

        let mut seen = HashSet::new();
        ready.retain(|t| seen.insert(t.id.clone()));
        ready.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });

        let mut exhausted_roles: HashSet<&'static str> = HashSet::new();

        for task in ready {
            if let Some(project_id) = project_filter
                && task.project_id != project_id
            {
                continue;
            }
            if !self.is_project_dispatch_enabled(&task.project_id) {
                continue;
            }

            let role = Self::role_for_task_status(&task.status);
            if exhausted_roles.contains(role) {
                continue;
            }
            let Some(model_ids) = role_models.get(role) else {
                tracing::debug!(task_id = %task.short_id, role, "CoordinatorActor: no model configured for task role");
                continue;
            };

            match self.supervisor.has_session(&task.id).await {
                Ok(true) => continue, // session already active
                Ok(false) => {}
                Err(SupervisorError::ActorDead) => {
                    tracing::error!("CoordinatorActor: supervisor actor dead, aborting dispatch");
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: has_session query failed"
                    );
                    continue;
                }
            }

            let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
                tracing::warn!(task_id = %task.short_id, project_id = %task.project_id, "CoordinatorActor: project path not found, skipping dispatch");
                continue;
            };

            let mut dispatched = false;
            let mut role_at_capacity = false;
            for model_id in model_ids {
                if !self.health.is_available(model_id) {
                    tracing::debug!(
                        model_id = %model_id,
                        task_id = %task.short_id,
                        "CoordinatorActor: model unavailable by health tracker"
                    );
                    continue;
                }

                match self
                    .supervisor
                    .dispatch(&task.id, &project_path, model_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            task_id = %task.short_id,
                            task_uuid = %task.id,
                            project_id = %task.project_id,
                            status = %task.status,
                            priority = task.priority,
                            role,
                            model_id = %model_id,
                            project_path,
                            "CoordinatorActor: task dispatched"
                        );
                        self.dispatched += 1;
                        dispatched = true;
                        break;
                    }
                    Err(SupervisorError::ModelAtCapacity { .. }) => {
                        role_at_capacity = true;
                        tracing::debug!(
                            task_id = %task.short_id,
                            model_id = %model_id,
                            "CoordinatorActor: model at capacity, trying next model"
                        );
                    }
                    Err(SupervisorError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: supervisor actor dead, aborting dispatch"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            model_id = %model_id,
                            error = %e,
                            "CoordinatorActor: dispatch failed"
                        );
                        break;
                    }
                }
            }

            if !dispatched {
                tracing::debug!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    role,
                    status = %task.status,
                    candidate_models = model_ids.len(),
                    role_at_capacity,
                    "CoordinatorActor: no model with available capacity for task"
                );
                if role_at_capacity {
                    exhausted_roles.insert(role);
                }
            }
        }

        let (batch_events_tx, _batch_events_rx) = broadcast::channel(1);
        let batch_repo = crate::db::repositories::epic_review_batch::EpicReviewBatchRepository::new(
            self.db.clone(),
            batch_events_tx,
        );
        let queued_anchors = match batch_repo
            .list_queued_anchors(project_filter, self.dispatch_limit as i64)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_queued_anchors failed");
                return;
            }
        };

        let Some(epic_models) = role_models.get("epic_reviewer") else {
            return;
        };

        for anchor in queued_anchors {
            if !self.is_project_dispatch_enabled(&anchor.project_id) {
                continue;
            }

            match self.supervisor.has_session(&anchor.task_id).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(SupervisorError::ActorDead) => {
                    tracing::error!("CoordinatorActor: supervisor actor dead, aborting dispatch");
                    return;
                }
                Err(e) => {
                    tracing::warn!(task_id = %anchor.task_id, error = %e, "CoordinatorActor: has_session failed for epic batch anchor");
                    continue;
                }
            }

            let Some(project_path) = self.project_path_for_id(&anchor.project_id).await else {
                continue;
            };

            for model_id in epic_models {
                if !self.health.is_available(model_id) {
                    continue;
                }

                match self
                    .supervisor
                    .dispatch(&anchor.task_id, &project_path, model_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(batch_id = %anchor.batch_id, epic_id = %anchor.epic_id, task_id = %anchor.task_id, model_id = %model_id, "CoordinatorActor: epic review batch dispatched");
                        self.dispatched += 1;
                        break;
                    }
                    Err(SupervisorError::ModelAtCapacity { .. }) => continue,
                    Err(SupervisorError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: supervisor actor dead, aborting dispatch"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(batch_id = %anchor.batch_id, task_id = %anchor.task_id, model_id = %model_id, error = %e, "CoordinatorActor: epic batch dispatch failed");
                        break;
                    }
                }
            }
        }
    }

    /// On each tick: find tasks in active execution states with no active session
    /// and release them back to a dispatch-ready state (AGENT-08).
    async fn detect_and_recover_stuck_filtered(&mut self, project_filter: Option<&str>) {
        let repo = self.task_repo();

        let mut any_recovered = false;
        for (status, action) in [
            ("in_progress", TransitionAction::Release),
            ("in_task_review", TransitionAction::ReleaseTaskReview),
        ] {
            let tasks = match repo.list_by_status(status).await {
                Ok(tasks) => tasks,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        status,
                        "CoordinatorActor: list_by_status failed during stuck check"
                    );
                    continue;
                }
            };

            for task in tasks {
                if let Some(project_id) = project_filter
                    && task.project_id != project_id
                {
                    continue;
                }

                match self.supervisor.has_session(&task.id).await {
                    Ok(true) => continue, // healthy — session is active
                    Ok(false) => {}
                    Err(SupervisorError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: supervisor actor dead during stuck check"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            status,
                            error = %e,
                            "CoordinatorActor: has_session failed during stuck check"
                        );
                        continue;
                    }
                }

                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    status,
                    transition_action = ?action,
                    "CoordinatorActor: stuck task detected (no session) — releasing"
                );
                match repo
                    .transition(
                        &task.id,
                        action.clone(),
                        "coordinator",
                        "system",
                        Some("stuck task — no active session detected"),
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            task_id = %task.short_id,
                            task_uuid = %task.id,
                            project_id = %task.project_id,
                            status,
                            transition_action = ?action,
                            "CoordinatorActor: released stuck task"
                        );
                        self.recovered += 1;
                        any_recovered = true;
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            status,
                            error = %e,
                            "CoordinatorActor: recovery transition failed"
                        );
                    }
                }
            }
        }

        let (batch_events_tx, _batch_events_rx) = broadcast::channel(1);
        let batch_repo = crate::db::repositories::epic_review_batch::EpicReviewBatchRepository::new(
            self.db.clone(),
            batch_events_tx,
        );
        let in_review = match batch_repo.list_in_review_anchors(project_filter).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_in_review_anchors failed during stuck check");
                Vec::new()
            }
        };

        for batch in in_review {
            if !self.is_project_dispatch_enabled(&batch.project_id) {
                continue;
            }
            match self.supervisor.has_session(&batch.task_id).await {
                Ok(true) => continue,
                Ok(false) => {
                    if let Err(e) = batch_repo
                        .requeue(
                            &batch.batch_id,
                            Some("stuck batch review — no active session detected"),
                        )
                        .await
                    {
                        tracing::warn!(batch_id = %batch.batch_id, error = %e, "CoordinatorActor: failed to requeue stuck epic review batch");
                    } else {
                        any_recovered = true;
                    }
                }
                Err(SupervisorError::ActorDead) => {
                    tracing::error!(
                        "CoordinatorActor: supervisor actor dead during batch stuck check"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(batch_id = %batch.batch_id, error = %e, "CoordinatorActor: has_session failed during batch stuck check");
                }
            }
        }

        // After releasing stuck tasks, immediately try to dispatch the now-open tasks.
        if any_recovered {
            self.dispatch_ready_tasks(None).await;
        }
    }

    /// Spawn background health-check tasks for all projects (or one) that have
    /// setup/verification commands configured (ADR-014, task bit0).
    async fn validate_all_project_health(&mut self, project_id_filter: Option<String>) {
        struct ProjectRow {
            id: String,
            path: String,
            setup_commands: String,
            verification_commands: String,
        }

        let rows: Vec<ProjectRow> = sqlx::query(
            "SELECT id, path, setup_commands, verification_commands FROM projects",
        )
        .fetch_all(self.db.pool())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            use sqlx::Row;
            ProjectRow {
                id: row.get("id"),
                path: row.get("path"),
                setup_commands: row.get("setup_commands"),
                verification_commands: row.get("verification_commands"),
            }
        })
        .collect();

        for row in rows {
            if let Some(ref filter) = project_id_filter {
                if row.id != *filter {
                    continue;
                }
            }

            let setup_cmds: Vec<CommandSpec> =
                serde_json::from_str(&row.setup_commands).unwrap_or_default();
            let verify_cmds: Vec<CommandSpec> =
                serde_json::from_str(&row.verification_commands).unwrap_or_default();

            if setup_cmds.is_empty() && verify_cmds.is_empty() {
                // No commands configured — always healthy; clear any stale failure.
                if self.unhealthy_projects.remove(&row.id).is_some() {
                    let _ = self.events_tx.send(DjinnEvent::ProjectHealthChanged {
                        project_id: row.id.clone(),
                        healthy: true,
                        error: None,
                    });
                }
                continue;
            }

            let sender = self.self_sender.clone();
            let db = self.db.clone();
            let events_tx = self.events_tx.clone();
            let project_id = row.id.clone();
            let path = row.path.clone();

            tracing::info!(
                project_id = %project_id,
                setup_count = setup_cmds.len(),
                verify_count = verify_cmds.len(),
                "CoordinatorActor: spawning project health check"
            );

            tokio::spawn(async move {
                let (healthy, error) =
                    match run_project_health_check(
                        project_id.clone(),
                        path,
                        setup_cmds,
                        verify_cmds,
                        db,
                        events_tx,
                    )
                    .await
                    {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e)),
                    };
                let _ = sender
                    .send(CoordinatorMessage::SetProjectHealth {
                        project_id,
                        healthy,
                        error,
                    })
                    .await;
            });
        }
    }

    fn task_repo(&self) -> TaskRepository {
        TaskRepository::new(self.db.clone(), self.events_tx.clone())
    }

    async fn project_path_for_id(&self, project_id: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?1")
            .bind(project_id)
            .fetch_optional(self.db.pool())
            .await
            .ok()
            .flatten()
    }
}

// ─── Project health check (ADR-014) ──────────────────────────────────────────

/// Create a temporary git worktree, run setup + verification commands, clean up,
/// and return `Ok(())` if all commands pass or `Err(reason)` if any fail.
async fn run_project_health_check(
    project_id: String,
    path: String,
    setup_cmds: Vec<CommandSpec>,
    verify_cmds: Vec<CommandSpec>,
    db: Database,
    events_tx: broadcast::Sender<DjinnEvent>,
) -> Result<(), String> {
    let project_path = std::path::PathBuf::from(&path);

    // Resolve target branch (falls back to "main").
    let target_branch = GitSettingsRepository::new(db, events_tx)
        .get(&project_id)
        .await
        .map(|s| s.target_branch)
        .unwrap_or_else(|_| "main".to_string());

    let git = GitActorHandle::spawn(project_path.clone())
        .map_err(|e| format!("failed to open git repo at {path}: {e}"))?;

    // Remove any stale health-check worktree from a previous crashed run.
    // Prune first to clear orphaned git metadata (directory may already be
    // gone but git worktree list still shows it), then force-remove the
    // directory if it still exists on disk.
    let _ = git
        .run_command(vec!["worktree".into(), "prune".into()])
        .await;
    let stale = project_path
        .join(".djinn")
        .join("worktrees")
        .join("_health_check");
    if stale.exists() {
        let _ = git.remove_worktree(&stale).await;
    }

    let wt_path = git
        .create_worktree("_health_check", &target_branch, true)
        .await
        .map_err(|e| format!("failed to create health-check worktree: {e}"))?;

    let result = async {
        if !setup_cmds.is_empty() {
            let results = run_commands(&setup_cmds, &wt_path)
                .await
                .map_err(|e| format!("setup error: {e}"))?;
            if let Some(f) = results.last().filter(|r| r.exit_code != 0) {
                return Err(format!(
                    "setup command '{}' failed (exit {}): {}",
                    f.name,
                    f.exit_code,
                    f.stderr.trim()
                ));
            }
        }
        if !verify_cmds.is_empty() {
            let results = run_commands(&verify_cmds, &wt_path)
                .await
                .map_err(|e| format!("verification error: {e}"))?;
            if let Some(f) = results.last().filter(|r| r.exit_code != 0) {
                return Err(format!(
                    "verification command '{}' failed (exit {}): {}",
                    f.name,
                    f.exit_code,
                    f.stderr.trim()
                ));
            }
        }
        Ok(())
    }
    .await;

    // Always remove the temporary worktree.
    if let Err(e) = git.remove_worktree(&wt_path).await {
        tracing::warn!(
            project_id = %project_id,
            error = %e,
            "CoordinatorActor: failed to remove health-check worktree"
        );
    }

    result
}

// ─── Handle ───────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle to the global `CoordinatorActor`.
#[derive(Clone)]
pub struct CoordinatorHandle {
    sender: mpsc::Sender<CoordinatorMessage>,
}

impl CoordinatorHandle {
    /// Spawn the `CoordinatorActor` and return a handle to it.
    pub fn spawn(
        events_tx: broadcast::Sender<DjinnEvent>,
        cancel: CancellationToken,
        db: Database,
        supervisor: AgentSupervisorHandle,
        catalog: CatalogService,
        health: HealthTracker,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let actor = CoordinatorActor::new(
            receiver,
            sender.clone(),
            events_tx,
            cancel,
            db,
            supervisor,
            catalog,
            health,
        );
        tokio::spawn(actor.run());
        Self { sender }
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

    /// Return the current coordinator status snapshot.
    pub async fn get_status(&self) -> Result<CoordinatorStatus, CoordinatorError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoordinatorMessage::GetStatus {
            project_id: None,
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| CoordinatorError::NoResponse)
    }

    pub async fn get_project_status(
        &self,
        project_id: &str,
    ) -> Result<CoordinatorStatus, CoordinatorError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoordinatorMessage::GetStatus {
            project_id: Some(project_id.to_owned()),
            respond_to: tx,
        })
        .await?;
        rx.await.map_err(|_| CoordinatorError::NoResponse)
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
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent::init_session_manager;
    use crate::db::repositories::epic::EpicRepository;
    use crate::db::repositories::task::TaskRepository;
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
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session_manager = init_session_manager(sessions_dir);
        let supervisor = AgentSupervisorHandle::spawn(app_state, session_manager, cancel.clone());
        let catalog = CatalogService::new();
        let health = HealthTracker::new();
        CoordinatorHandle::spawn(tx.clone(), cancel, db.clone(), supervisor, catalog, health)
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

    #[tokio::test]
    async fn initial_status_is_paused() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let status = handle.get_status().await.unwrap();
        assert!(status.paused, "coordinator should start paused by default");
        assert_eq!(status.tasks_dispatched, 0);
        assert_eq!(status.sessions_recovered, 0);
    }

    // ── Pause / Resume ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn pause_and_resume_toggle_state() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        handle.pause().await.unwrap();
        let status = handle.get_status().await.unwrap();
        assert!(status.paused, "should be paused");

        handle.resume().await.unwrap();
        let status = handle.get_status().await.unwrap();
        assert!(!status.paused, "should be resumed");
    }

    #[tokio::test]
    async fn trigger_dispatch_while_paused_is_a_noop() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        handle.pause().await.unwrap();
        handle.trigger_dispatch().await.unwrap();

        let status = handle.get_status().await.unwrap();
        // tasks_dispatched stays 0 because the supervisor stub is a no-op,
        // but the coordinator shouldn't even attempt dispatch while paused.
        assert_eq!(status.tasks_dispatched, 0);
    }

    // ── Dispatch on open-task event ──────────────────────────────────────────

    #[tokio::test]
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
        handle.resume().await.unwrap();
        handle.trigger_dispatch().await.unwrap();

        // Give the actor time to process the message and run dispatch.
        let status = handle.get_status().await.unwrap();
        // Supervisor stub says no session → dispatch is called once.
        assert_eq!(
            status.tasks_dispatched, 1,
            "should have dispatched the ready task"
        );
    }

    #[tokio::test]
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
        handle.resume().await.unwrap();
        handle.trigger_dispatch().await.unwrap();

        let status = handle.get_status().await.unwrap();
        assert_eq!(
            status.tasks_dispatched, 1,
            "should dispatch task waiting for review"
        );
    }

    // ── Stuck detection ───────────────────────────────────────────────────────

    #[tokio::test]
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
        handle.resume().await.unwrap();
        handle.trigger_stuck_scan().await.unwrap();

        let status = handle.get_status().await.unwrap();
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
