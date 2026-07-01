use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;

use super::super::SlotPoolConfig;
#[cfg(any(test, feature = "test-support"))]
use super::types::SlotFactory;
use super::types::{PoolError, PoolStatus, RunningTaskInfo};

/// Compatibility wrapper around the canonical `djinn-slot` pool handle.
///
/// The duplicated pool actor and message-handling behavior have moved to
/// `djinn-slot`. `djinn-agent` only converts `AgentContext` to `SlotContext` at
/// construction time and delegates every pool operation to the canonical handle.
#[derive(Clone)]
pub struct SlotPoolHandle {
    inner: djinn_slot::SlotPoolHandle,
}

impl SlotPoolHandle {
    pub fn spawn(
        app_state: AgentContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
    ) -> Self {
        let slot_context = super::super::host_callbacks::agent_to_dispatch_slot_context(&app_state);
        Self {
            inner: djinn_slot::SlotPoolHandle::spawn(slot_context, cancel, config),
        }
    }

    pub(crate) fn into_djinn_slot(self) -> Option<djinn_slot::SlotPoolHandle> {
        Some(self.inner)
    }

    pub fn try_into_djinn_slot(self) -> Result<djinn_slot::SlotPoolHandle, PoolError> {
        Ok(self.inner)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_with_factory(
        app_state: AgentContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
        slot_factory: SlotFactory,
    ) -> Self {
        let slot_context = super::super::host_callbacks::agent_to_dispatch_slot_context(&app_state);
        let agent_state = app_state.clone();
        let canonical_factory: djinn_slot::SlotFactory =
            std::sync::Arc::new(move |slot_id, model_id, event_tx, _slot_ctx, cancel| {
                slot_factory(slot_id, model_id, event_tx, agent_state.clone(), cancel)
                    .into_djinn_slot()
            });
        Self {
            inner: djinn_slot::SlotPoolHandle::spawn_with_factory(
                slot_context,
                cancel,
                config,
                canonical_factory,
            ),
        }
    }

    pub async fn dispatch(
        &self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        self.inner.dispatch(task_id, project_path, model_id).await
    }

    pub async fn has_session(&self, task_id: &str) -> Result<bool, PoolError> {
        self.inner.has_session(task_id).await
    }

    pub async fn kill_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.inner.kill_session(task_id).await
    }

    pub async fn terminate_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.inner.terminate_session(task_id).await
    }

    pub async fn evict_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.inner.evict_session(task_id).await
    }

    pub async fn pause_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.inner.pause_session(task_id).await
    }

    pub async fn get_status(&self) -> Result<PoolStatus, PoolError> {
        self.inner.get_status().await
    }

    pub async fn snapshot(
        &self,
    ) -> Result<Vec<djinn_orchestration_types::coordinator::DebugSlot>, PoolError> {
        self.inner.snapshot().await
    }

    pub async fn session_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<RunningTaskInfo>, PoolError> {
        self.inner.session_for_task(task_id).await
    }

    pub async fn reconfigure(&self, config: SlotPoolConfig) -> Result<(), PoolError> {
        self.inner.reconfigure(config).await
    }

    pub async fn interrupt_all(&self, reason: &str) -> Result<(), PoolError> {
        self.inner.interrupt_all(reason).await
    }

    pub async fn interrupt_project(&self, project_id: &str, reason: &str) -> Result<(), PoolError> {
        self.inner.interrupt_project(project_id, reason).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn test_set_token_override(&self, task_id: &str, token_count: u64, turn_count: u64) {
        self.inner
            .test_set_token_override(task_id, token_count, turn_count)
            .await;
    }
}

impl TryFrom<SlotPoolHandle> for djinn_slot::SlotPoolHandle {
    type Error = PoolError;

    fn try_from(handle: SlotPoolHandle) -> Result<Self, Self::Error> {
        handle.try_into_djinn_slot()
    }
}
