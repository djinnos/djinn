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

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{self, Interval};
use tokio_util::sync::CancellationToken;

use crate::actors::supervisor::{AgentSupervisorHandle, SupervisorError};
use crate::db::connection::Database;
use crate::db::repositories::task::{ReadyQuery, TaskRepository};
use crate::events::DjinnEvent;
use crate::models::task::TransitionAction;
use crate::provider::health::HealthTracker;

/// Interval between stuck-detection passes (AGENT-08).
const STUCK_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const DEFAULT_MODEL_ID: &str = "openai/gpt-4o-mini";
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
    Pause,
    /// Resume dispatch and immediately run a dispatch pass.
    Resume,
    /// Return current coordinator status.
    GetStatus {
        respond_to: Reply<CoordinatorStatus>,
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
    #[allow(dead_code)] // will be used by d9s4 model-availability checks
    health: HealthTracker,
    // State
    paused: bool,
    // Metrics
    dispatched: u64,
    recovered: u64,
}

// Field count: receiver, events, cancel, tick, db, events_tx, supervisor,
//              health, paused, dispatched, recovered = 11 ✓ (≤20)

impl CoordinatorActor {
    fn new(
        receiver: mpsc::Receiver<CoordinatorMessage>,
        events_tx: broadcast::Sender<DjinnEvent>,
        cancel: CancellationToken,
        db: Database,
        supervisor: AgentSupervisorHandle,
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
            health,
            paused: false,
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
            CoordinatorMessage::Pause => {
                if !self.paused {
                    tracing::info!("CoordinatorActor: paused");
                    self.paused = true;
                    if let Err(e) = self
                        .supervisor
                        .interrupt_all("session interrupted by coordinator pause")
                        .await
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

    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    async fn dispatch_ready_tasks(&mut self) {
        let repo = self.task_repo();
        let mut ready = match repo
            .list_ready(ReadyQuery {
                issue_type: Some("!epic".into()),
                limit: 50,
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

        for status in ["needs_task_review", "needs_phase_review"] {
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
            if !self.health.is_available(DEFAULT_MODEL_ID) {
                tracing::warn!(
                    model_id = DEFAULT_MODEL_ID,
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

            tracing::info!(task_id = %task.short_id, "CoordinatorActor: dispatching task");
            match self.supervisor.dispatch(&task.id, DEFAULT_MODEL_ID).await {
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
        health: HealthTracker,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let actor = CoordinatorActor::new(receiver, events_tx, cancel, db, supervisor, health);
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
        self.send(CoordinatorMessage::Pause).await
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
        let health = HealthTracker::new();
        CoordinatorHandle::spawn(tx.clone(), cancel, db.clone(), supervisor, health)
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
    async fn initial_status_is_not_paused() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let status = handle.get_status().await.unwrap();
        assert!(!status.paused);
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
