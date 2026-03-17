use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::context::AgentContext;

use super::super::SlotPoolConfig;
use super::actor::SlotPool;
#[cfg(test)]
use super::types::SlotFactory;
use super::types::{PoolError, PoolMessage, PoolStatus, Reply, RunningTaskInfo};

#[derive(Clone)]
pub struct SlotPoolHandle {
    sender: mpsc::Sender<PoolMessage>,
}

impl SlotPoolHandle {
    pub fn spawn(app_state: AgentContext, cancel: CancellationToken, config: SlotPoolConfig) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(SlotPool::new(receiver, app_state, cancel, config).run());
        Self { sender }
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_factory(
        app_state: AgentContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
        slot_factory: SlotFactory,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(
            SlotPool::new_with_factory(receiver, app_state, cancel, config, slot_factory).run(),
        );
        Self { sender }
    }

    async fn request<T>(&self, f: impl FnOnce(Reply<T>) -> PoolMessage) -> Result<T, PoolError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(f(tx))
            .await
            .map_err(|_| PoolError::ActorDead)?;
        rx.await.map_err(|_| PoolError::NoResponse)?
    }

    pub async fn dispatch(
        &self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::Dispatch {
            task_id: task_id.to_owned(),
            project_path: project_path.to_owned(),
            model_id: model_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn dispatch_project(
        &self,
        project_id: &str,
        project_path: &str,
        agent_type: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::DispatchProject {
            project_id: project_id.to_owned(),
            project_path: project_path.to_owned(),
            agent_type: agent_type.to_owned(),
            model_id: model_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn has_session(&self, task_id: &str) -> Result<bool, PoolError> {
        self.request(|tx| PoolMessage::HasSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn kill_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::KillSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn pause_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::PauseSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn get_status(&self) -> Result<PoolStatus, PoolError> {
        self.request(|tx| PoolMessage::GetStatus { respond_to: tx })
            .await
    }

    pub async fn session_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<RunningTaskInfo>, PoolError> {
        self.request(|tx| PoolMessage::GetSessionForTask {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn reconfigure(&self, config: SlotPoolConfig) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::Reconfigure {
            config,
            respond_to: tx,
        })
        .await
    }

    pub async fn interrupt_all(&self, reason: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::InterruptAll {
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn interrupt_project(&self, project_id: &str, reason: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::InterruptProject {
            project_id: project_id.to_owned(),
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }
}
