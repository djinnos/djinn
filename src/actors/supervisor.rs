// AgentSupervisor — 1x global, manages in-process Goose session lifecycle.
//
// Ryhl hand-rolled actor pattern:
//   - `AgentSupervisorHandle` (mpsc sender) is the public API.
//   - `AgentSupervisor` (mpsc receiver) runs in a dedicated tokio task.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{GooseSessionHandle, SessionManager};
use crate::server::AppState;

// ─── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
    #[error("session already active for task {task_id}")]
    SessionAlreadyActive { task_id: String },
    #[error("model {model_id} at capacity ({active}/{max})")]
    ModelAtCapacity {
        model_id: String,
        active: u32,
        max: u32,
    },
}

// ─── Status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SupervisorStatus {
    pub active_sessions: usize,
    pub capacity: HashMap<String, ModelCapacity>,
}

#[derive(Debug, Clone)]
pub struct ModelCapacity {
    pub active: u32,
    pub max: u32,
}

// ─── Messages ─────────────────────────────────────────────────────────────────

type Reply<T> = oneshot::Sender<Result<T, SupervisorError>>;

enum SupervisorMessage {
    /// Start a new session for `task_id` with `model_id`.
    Dispatch {
        task_id: String,
        model_id: String,
        respond_to: Reply<()>,
    },
    /// Check whether there is an active session for `task_id`.
    HasSession {
        task_id: String,
        respond_to: Reply<bool>,
    },
    /// Cancel and remove a running session.
    KillSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    /// Return current session/capacity snapshot.
    GetStatus {
        respond_to: Reply<SupervisorStatus>,
    },
    /// Internal message sent by session tasks when they finish.
    SessionCompleted {
        task_id: String,
    },
}

// ─── Actor ────────────────────────────────────────────────────────────────────

struct AgentSupervisor {
    receiver: mpsc::Receiver<SupervisorMessage>,
    sessions: HashMap<String, GooseSessionHandle>,
    capacity: HashMap<String, ModelCapacity>,
    session_models: HashMap<String, String>,
    #[allow(dead_code)]
    session_manager: Arc<SessionManager>,
    #[allow(dead_code)]
    app_state: AppState,
    cancel: CancellationToken,
    sender: mpsc::Sender<SupervisorMessage>,
}

impl AgentSupervisor {
    fn new(
        receiver: mpsc::Receiver<SupervisorMessage>,
        sender: mpsc::Sender<SupervisorMessage>,
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            receiver,
            sessions: HashMap::new(),
            capacity: HashMap::new(),
            session_models: HashMap::new(),
            session_manager,
            app_state,
            cancel,
            sender,
        }
    }

    async fn run(mut self) {
        tracing::info!("AgentSupervisor started");
        while let Some(msg) = self.receiver.recv().await {
            self.handle(msg).await;
        }
        tracing::info!("AgentSupervisor stopped");
    }

    async fn handle(&mut self, msg: SupervisorMessage) {
        match msg {
            SupervisorMessage::Dispatch {
                task_id,
                model_id,
                respond_to,
            } => {
                let _ = respond_to.send(self.dispatch(task_id, model_id));
            }
            SupervisorMessage::HasSession { task_id, respond_to } => {
                let _ = respond_to.send(Ok(self.sessions.contains_key(&task_id)));
            }
            SupervisorMessage::KillSession { task_id, respond_to } => {
                let _ = respond_to.send(self.kill_session(task_id));
            }
            SupervisorMessage::GetStatus { respond_to } => {
                let _ = respond_to.send(Ok(SupervisorStatus {
                    active_sessions: self.sessions.len(),
                    capacity: self.capacity.clone(),
                }));
            }
            SupervisorMessage::SessionCompleted { task_id } => {
                self.remove_session(&task_id);
            }
        }
    }

    fn dispatch(&mut self, task_id: String, model_id: String) -> Result<(), SupervisorError> {
        if self.sessions.contains_key(&task_id) {
            return Err(SupervisorError::SessionAlreadyActive { task_id });
        }

        let entry =
            self.capacity.entry(model_id.clone()).or_insert(ModelCapacity { active: 0, max: 1 });
        if entry.active >= entry.max {
            return Err(SupervisorError::ModelAtCapacity {
                model_id,
                active: entry.active,
                max: entry.max,
            });
        }

        entry.active += 1;
        let session_cancel = CancellationToken::new();
        let session_cancel_for_join = session_cancel.clone();
        let global_cancel = self.cancel.clone();
        let task_id_for_join = task_id.clone();
        let sender = self.sender.clone();

        let join = tokio::spawn(async move {
            tokio::select! {
                _ = session_cancel_for_join.cancelled() => {}
                _ = global_cancel.cancelled() => {}
            }
            let _ = sender
                .send(SupervisorMessage::SessionCompleted { task_id: task_id_for_join })
                .await;
            Ok(())
        });

        self.sessions.insert(
            task_id.clone(),
            GooseSessionHandle {
                join,
                cancel: session_cancel,
                session_id: format!("session-{task_id}"),
                task_id: task_id.clone(),
            },
        );
        self.session_models.insert(task_id, model_id);
        Ok(())
    }

    fn kill_session(&mut self, task_id: String) -> Result<(), SupervisorError> {
        if let Some(handle) = self.sessions.remove(&task_id) {
            handle.cancel.cancel();
            self.decrement_capacity(&task_id);
            self.session_models.remove(&task_id);
        }
        Ok(())
    }

    fn remove_session(&mut self, task_id: &str) {
        if self.sessions.remove(task_id).is_some() {
            self.decrement_capacity(task_id);
            self.session_models.remove(task_id);
        }
    }

    fn decrement_capacity(&mut self, task_id: &str) {
        if let Some(model_id) = self.session_models.get(task_id)
            && let Some(model_capacity) = self.capacity.get_mut(model_id)
            && model_capacity.active > 0
        {
            model_capacity.active -= 1;
        }
    }
}

// ─── Handle ───────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle to the global `AgentSupervisor` actor.
#[derive(Clone)]
pub struct AgentSupervisorHandle {
    sender: mpsc::Sender<SupervisorMessage>,
}

impl AgentSupervisorHandle {
    /// Spawn the AgentSupervisor and return a handle to it.
    pub fn spawn(
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(
            AgentSupervisor::new(
                receiver,
                sender.clone(),
                app_state,
                session_manager,
                cancel,
            )
            .run(),
        );
        Self { sender }
    }

    async fn request<T>(
        &self,
        f: impl FnOnce(Reply<T>) -> SupervisorMessage,
    ) -> Result<T, SupervisorError> {
        let (tx, rx) = oneshot::channel();
        self.sender.send(f(tx)).await.map_err(|_| SupervisorError::ActorDead)?;
        rx.await.map_err(|_| SupervisorError::NoResponse)?
    }

    /// Return `true` if there is an active session for the given task.
    pub async fn has_session(&self, task_id: &str) -> Result<bool, SupervisorError> {
        self.request(|tx| SupervisorMessage::HasSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    /// Start a new agent session for the given task.
    pub async fn dispatch(&self, task_id: &str, model_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::Dispatch {
            task_id: task_id.to_owned(),
            model_id: model_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    /// Cancel a running agent session for the given task.
    pub async fn kill_session(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::KillSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    /// Return current session and capacity status snapshot.
    pub async fn get_status(&self) -> Result<SupervisorStatus, SupervisorError> {
        self.request(|tx| SupervisorMessage::GetStatus { respond_to: tx }).await
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::agent::init_session_manager;
    use crate::test_helpers;

    fn spawn_supervisor(temp: &TempDir) -> AgentSupervisorHandle {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db, cancel.clone());
        let sessions = init_session_manager(temp.path().to_path_buf());
        AgentSupervisorHandle::spawn(app_state, sessions, cancel)
    }

    #[tokio::test]
    async fn tracks_session_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);

        assert!(!supervisor.has_session("task-1").await.unwrap());
        supervisor.dispatch("task-1", "model-a").await.unwrap();
        assert!(supervisor.has_session("task-1").await.unwrap());

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(!supervisor.has_session("task-1").await.unwrap());
    }

    #[tokio::test]
    async fn enforces_per_model_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);

        supervisor.dispatch("task-1", "model-a").await.unwrap();
        let err = supervisor.dispatch("task-2", "model-a").await.unwrap_err();
        assert!(matches!(err, SupervisorError::ModelAtCapacity { .. }));

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        supervisor.dispatch("task-2", "model-a").await.unwrap();
    }

    #[tokio::test]
    async fn status_reports_capacity_and_active_count() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);

        supervisor.dispatch("task-1", "model-a").await.unwrap();
        let status = supervisor.get_status().await.unwrap();
        assert_eq!(status.active_sessions, 1);
        let model = status.capacity.get("model-a").unwrap();
        assert_eq!(model.active, 1);
        assert_eq!(model.max, 1);
    }
}
