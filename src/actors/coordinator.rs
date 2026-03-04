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

use crate::actors::supervisor::{AgentSupervisorHandle, SupervisorError};
use crate::db::connection::Database;
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
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
}

// ─── Messages (≤15 variants — AGENT-11) ──────────────────────────────────────

type Reply<T> = oneshot::Sender<T>;

enum CoordinatorMessage {
    /// Run an immediate dispatch pass for all ready tasks.
    TriggerDispatch,
    /// Pause dispatch — no new sessions will start until `Resume`.
    Pause {
        /// If true, interrupt all active sessions immediately.
        interrupt_active: bool,
        /// Optional interruption reason passed to the supervisor.
        reason: String,
    },
    /// Resume dispatch and immediately run a dispatch pass.
    Resume,
    /// Return current coordinator status.
    GetStatus {
        respond_to: Reply<CoordinatorStatus>,
    },
    /// Update runtime dispatch limit from settings reload.
    UpdateDispatchLimit { limit: usize },
    /// Update per-role model priority list from settings reload.
    UpdateModelPriorities {
        priorities: HashMap<String, Vec<String>>,
    },
    /// Run an immediate stuck-task detection pass.
    TriggerStuckScan,
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
    // State
    paused: bool,
    dispatch_limit: usize,
    model_priorities: HashMap<String, Vec<String>>,
    // Metrics
    dispatched: u64,
    recovered: u64,
}

// Field count: receiver, events, cancel, tick, db, events_tx, supervisor,
//              catalog, health, paused, dispatched, recovered = 12 ✓ (≤20)

impl CoordinatorActor {
    fn new(
        receiver: mpsc::Receiver<CoordinatorMessage>,
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
            paused: true,
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
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
                        self.detect_and_recover_stuck().await;
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
                    self.dispatch_ready_tasks().await;
                }
            }
            CoordinatorMessage::Pause {
                interrupt_active,
                reason,
            } => {
                if !self.paused {
                    tracing::info!("CoordinatorActor: paused");
                    self.paused = true;
                    if interrupt_active && let Err(e) = self.supervisor.interrupt_all(&reason).await
                    {
                        tracing::warn!(error = %e, "CoordinatorActor: failed to interrupt active sessions on pause");
                    }
                }
            }
            CoordinatorMessage::Resume => {
                if self.paused {
                    tracing::info!("CoordinatorActor: resumed");
                    self.paused = false;
                    self.dispatch_ready_tasks().await;
                }
            }
            CoordinatorMessage::GetStatus { respond_to } => {
                let _ = respond_to.send(CoordinatorStatus {
                    paused: self.paused,
                    tasks_dispatched: self.dispatched,
                    sessions_recovered: self.recovered,
                });
            }
            CoordinatorMessage::TriggerStuckScan => {
                if !self.paused {
                    self.detect_and_recover_stuck().await;
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
                    self.dispatch_ready_tasks().await;
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
            // A task was created or transitioned back to open → check dispatch.
            DjinnEvent::TaskCreated(t) | DjinnEvent::TaskUpdated(t) if t.status == "open" => {
                tracing::debug!(
                    task_id = %t.short_id,
                    "CoordinatorActor: open-task event → dispatch pass"
                );
                self.dispatch_ready_tasks().await;
            }
            _ => {}
        }
    }

    /// Resolve dispatch model for a given role from configured priorities,
    /// falling back to first credential-backed tool-capable model.
    async fn resolve_dispatch_model_for_role(&self, _role: &str) -> Option<String> {
        #[cfg(test)]
        {
            return Some(DEFAULT_MODEL_ID.to_owned());
        }

        #[cfg(not(test))]
        {
            let cred_repo = crate::db::repositories::credential::CredentialRepository::new(
                self.db.clone(),
                self.events_tx.clone(),
            );
            let credentials = cred_repo.list().await.ok()?;
            if credentials.is_empty() {
                return None;
            }

            let credential_provider_ids: HashSet<String> =
                credentials.iter().map(|c| c.provider_id.clone()).collect();

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
                        if exists {
                            return Some(configured.clone());
                        }
                        continue;
                    }

                    if credential_provider_ids.contains(configured) {
                        let models = self.catalog.list_models(configured);
                        if let Some(model) = models.iter().find(|m| m.tool_call) {
                            return Some(format!("{configured}/{}", model.id));
                        }
                        if let Some(model) = models.first() {
                            return Some(format!("{configured}/{}", model.id));
                        }
                    }
                }
            }

            for cred in &credentials {
                let models = self.catalog.list_models(&cred.provider_id);
                // Prefer a model with tool_call capability.
                if let Some(model) = models.iter().find(|m| m.tool_call) {
                    return Some(format!("{}/{}", cred.provider_id, model.id));
                }
                // Fall back to first model if none has tool_call.
                if let Some(model) = models.first() {
                    return Some(format!("{}/{}", cred.provider_id, model.id));
                }
            }

            None
        }
    }

    fn role_for_task_status(status: &str) -> &'static str {
        match status {
            "needs_task_review" | "in_task_review" => "task_reviewer",
            "needs_epic_review" | "in_epic_review" => "epic_reviewer",
            _ => "worker",
        }
    }

    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    async fn dispatch_ready_tasks(&mut self) {
        let mut role_models: HashMap<&'static str, String> = HashMap::new();
        for role in ["worker", "task_reviewer", "epic_reviewer"] {
            if let Some(model_id) = self.resolve_dispatch_model_for_role(role).await {
                role_models.insert(role, model_id);
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

        for status in ["needs_task_review", "needs_epic_review"] {
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

        for task in ready {
            let role = Self::role_for_task_status(&task.status);
            let Some(model_id) = role_models.get(role) else {
                tracing::debug!(task_id = %task.short_id, role, "CoordinatorActor: no model configured for task role");
                continue;
            };

            if !self.health.is_available(&model_id) {
                tracing::warn!(
                    model_id = %model_id,
                    task_id = %task.short_id,
                    "CoordinatorActor: model unavailable by health tracker, skipping dispatch"
                );
                continue;
            }

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

            tracing::info!(task_id = %task.short_id, model_id = %model_id, "CoordinatorActor: dispatching task");
            let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
                tracing::warn!(task_id = %task.short_id, project_id = %task.project_id, "CoordinatorActor: project path not found, skipping dispatch");
                continue;
            };
            match self
                .supervisor
                .dispatch(&task.id, &project_path, model_id)
                .await
            {
                Ok(()) => self.dispatched += 1,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: dispatch failed"
                    );
                }
            }
        }
    }

    /// On each tick: find `in_progress` tasks with no active session and release
    /// them back to `open` so they can be re-dispatched (AGENT-08).
    async fn detect_and_recover_stuck(&mut self) {
        let repo = self.task_repo();
        let in_progress = match repo.list_by_status("in_progress").await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_by_status(in_progress) failed");
                return;
            }
        };

        let mut any_recovered = false;
        for task in in_progress {
            match self.supervisor.has_session(&task.id).await {
                Ok(true) => continue, // healthy — session is active
                Ok(false) => {}
                Err(SupervisorError::ActorDead) => {
                    tracing::error!("CoordinatorActor: supervisor actor dead during stuck check");
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: has_session failed during stuck check"
                    );
                    continue;
                }
            }

            tracing::warn!(
                task_id = %task.short_id,
                "CoordinatorActor: stuck task detected (in_progress, no session) — releasing"
            );
            match repo
                .transition(
                    &task.id,
                    TransitionAction::Release,
                    "coordinator",
                    "system",
                    Some("stuck task — no active session detected"),
                    None,
                )
                .await
            {
                Ok(_) => {
                    self.recovered += 1;
                    any_recovered = true;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: recovery transition failed"
                    );
                }
            }
        }

        // After releasing stuck tasks, immediately try to dispatch the now-open tasks.
        if any_recovered {
            self.dispatch_ready_tasks().await;
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
        let actor =
            CoordinatorActor::new(receiver, events_tx, cancel, db, supervisor, catalog, health);
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

    /// Pause dispatch (no new sessions will start).
    pub async fn pause(&self) -> Result<(), CoordinatorError> {
        self.send(CoordinatorMessage::Pause {
            interrupt_active: false,
            reason: "session interrupted by coordinator pause".to_string(),
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

    /// Return the current coordinator status snapshot.
    pub async fn get_status(&self) -> Result<CoordinatorStatus, CoordinatorError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoordinatorMessage::GetStatus { respond_to: tx })
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
