#[cfg(any(test, feature = "test-support"))]
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;

use super::SlotEvent;

/// Agent-facade lifecycle runner retained for test-support callers that build
/// slots with an `AgentContext`. Production actor behavior is owned by
/// `djinn_slot::SlotHandle`; this type is adapted into the canonical runner in
/// [`SlotHandle::spawn_with_test_runner`].
#[cfg(any(test, feature = "test-support"))]
type LifecycleFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'static>>;
#[cfg(any(test, feature = "test-support"))]
pub type TestLifecycleRunner = Arc<
    dyn Fn(
            String,
            String,
            String,
            AgentContext,
            CancellationToken,
            CancellationToken,
            Option<serde_json::Value>,
        ) -> LifecycleFuture
        + Send
        + Sync,
>;

/// Compatibility wrapper around the canonical `djinn-slot` slot handle.
///
/// The duplicated actor state machine was removed from `djinn-agent`; all run,
/// kill, pause, drain, and completion-event behavior now executes in
/// `djinn_slot::SlotHandle`. This wrapper only converts the host
/// [`AgentContext`] into a `djinn_slot::host::SlotContext` at spawn time so the
/// historical `djinn_agent::actors::slot::SlotHandle` API remains available.
#[derive(Debug, Clone)]
pub struct SlotHandle {
    inner: djinn_slot::SlotHandle,
}

impl SlotHandle {
    pub fn spawn(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: AgentContext,
        cancel: CancellationToken,
    ) -> Self {
        let slot_ctx = super::host_callbacks::agent_to_dispatch_slot_context(&app_state);
        Self {
            inner: djinn_slot::SlotHandle::spawn(id, model_id, event_tx, slot_ctx, cancel),
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_with_test_runner(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: AgentContext,
        cancel: CancellationToken,
        runner: TestLifecycleRunner,
    ) -> Self {
        let slot_ctx = super::host_callbacks::agent_to_dispatch_slot_context(&app_state);
        let agent_state = app_state.clone();
        let canonical_runner: djinn_slot::TestLifecycleRunner = Arc::new(
            move |task_id,
                  project_path,
                  model_id,
                  _slot_ctx,
                  kill,
                  pause,
                  resume_lifecycle_metadata| {
                runner(
                    task_id,
                    project_path,
                    model_id,
                    agent_state.clone(),
                    kill,
                    pause,
                    resume_lifecycle_metadata,
                )
            },
        );
        Self {
            inner: djinn_slot::SlotHandle::spawn_with_test_runner(
                id,
                model_id,
                event_tx,
                slot_ctx,
                cancel,
                canonical_runner,
            ),
        }
    }
    pub fn id(&self) -> usize {
        self.inner.id()
    }
    pub fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    pub async fn run_task(
        &self,
        task_id: String,
        project_path: String,
    ) -> Result<(), djinn_slot::SlotError> {
        self.inner.run_task(task_id, project_path).await
    }
    pub async fn kill(&self) -> Result<(), djinn_slot::SlotError> {
        self.inner.kill().await
    }
    pub async fn pause(&self) -> Result<(), djinn_slot::SlotError> {
        self.inner.pause().await
    }
    pub async fn drain(&self) -> Result<(), djinn_slot::SlotError> {
        self.inner.drain().await
    }
    #[cfg(any(test, feature = "test-support"))]
    /// Expose the canonical production slot for downstream integration tests.
    /// The facade's `spawn` path has already installed the real host callback,
    /// including `supervisor_runner::dispatch_task_runtime` persistence.
    pub fn into_djinn_slot(self) -> djinn_slot::SlotHandle {
        self.inner
    }
}
