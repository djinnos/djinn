// AgentSupervisor — 1x global, manages agent subprocess lifecycle.
//
// Full session management is implemented in task d9s4. This stub exposes the
// handle interface needed by CoordinatorActor so the two actors can be wired
// together without a circular compilation dependency.

use tokio::sync::{mpsc, oneshot};

// ─── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
}

// ─── Messages ─────────────────────────────────────────────────────────────────

type Reply<T> = oneshot::Sender<Result<T, SupervisorError>>;

enum SupervisorMessage {
    /// Check whether there is an active session for `task_id`.
    HasSession {
        #[allow(dead_code)] // session lookup by ID is d9s4
        task_id: String,
        respond_to: Reply<bool>,
    },
    /// Dispatch (start) a new session for `task_id`.
    Dispatch { task_id: String, respond_to: Reply<()> },
}

// ─── Actor ────────────────────────────────────────────────────────────────────

struct AgentSupervisor {
    receiver: mpsc::Receiver<SupervisorMessage>,
}

impl AgentSupervisor {
    fn new(receiver: mpsc::Receiver<SupervisorMessage>) -> Self {
        Self { receiver }
    }

    async fn run(mut self) {
        tracing::debug!("AgentSupervisor started (stub)");
        while let Some(msg) = self.receiver.recv().await {
            self.handle(msg);
        }
        tracing::debug!("AgentSupervisor stopped");
    }

    fn handle(&mut self, msg: SupervisorMessage) {
        match msg {
            SupervisorMessage::HasSession { task_id: _, respond_to } => {
                // Stub: no sessions tracked yet (task d9s4).
                let _ = respond_to.send(Ok(false));
            }
            SupervisorMessage::Dispatch { task_id, respond_to } => {
                tracing::info!(task_id, "AgentSupervisor: dispatch (stub — no-op until d9s4)");
                let _ = respond_to.send(Ok(()));
            }
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
    pub fn spawn() -> Self {
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(AgentSupervisor::new(receiver).run());
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
    pub async fn dispatch(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::Dispatch {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }
}
