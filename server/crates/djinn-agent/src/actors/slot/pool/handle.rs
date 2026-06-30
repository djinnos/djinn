use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use djinn_orchestration_types::coordinator::DebugSlot;

use super::super::SlotPoolConfig;
#[cfg(any(test, feature = "test-support"))]
use super::actor::SlotPool;
#[cfg(any(test, feature = "test-support"))]
use super::types::SlotFactory;
use super::types::{PoolError, PoolMessage, PoolStatus, Reply, RunningTaskInfo};

#[derive(Clone)]
enum SlotPoolInner {
    Canonical(djinn_slot::SlotPoolHandle),
    #[cfg(any(test, feature = "test-support"))]
    Legacy(mpsc::Sender<PoolMessage>),
}

#[derive(Clone)]
pub struct SlotPoolHandle {
    inner: SlotPoolInner,
}

impl SlotPoolHandle {
    pub fn spawn(
        app_state: AgentContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
    ) -> Self {
        let slot_context = super::super::host_callbacks::agent_to_dispatch_slot_context(&app_state);
        Self {
            inner: SlotPoolInner::Canonical(djinn_slot::SlotPoolHandle::spawn(
                slot_context,
                cancel,
                config,
            )),
        }
    }

    pub(crate) fn into_djinn_slot(self) -> Option<djinn_slot::SlotPoolHandle> {
        self.try_into_djinn_slot().ok()
    }

    /// Return the canonical `djinn-slot` pool handle wrapped by this agent
    /// compatibility facade.
    ///
    /// Production agent pool entry points construct a `SlotContext` via
    /// `host_callbacks::agent_to_dispatch_slot_context` and spawn the canonical
    /// `djinn_slot::SlotPoolHandle` directly.  This accessor is the
    /// compile-checkable handoff seam for downstream facades that are ready to
    /// operate on the canonical slot-pool API.  Test-only `spawn_with_factory`
    /// handles still use the legacy in-crate white-box actor so old agent tests
    /// can inject an `AgentContext`-based slot factory; those handles cannot be
    /// converted.
    pub fn try_into_djinn_slot(self) -> Result<djinn_slot::SlotPoolHandle, PoolError> {
        match self.inner {
            SlotPoolInner::Canonical(handle) => Some(handle),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => None,
        }
        .ok_or(PoolError::ActorDead)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_with_factory(
        app_state: AgentContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
        slot_factory: SlotFactory,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(
            SlotPool::new_with_factory(receiver, app_state, cancel, config, slot_factory).run(),
        );
        Self {
            inner: SlotPoolInner::Legacy(sender),
        }
    }

    async fn request<T>(&self, f: impl FnOnce(Reply<T>) -> PoolMessage) -> Result<T, PoolError> {
        let (tx, rx) = oneshot::channel::<Result<T, PoolError>>();
        match &self.inner {
            SlotPoolInner::Canonical(_) => Err(PoolError::ActorDead),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(sender) => {
                sender.send(f(tx)).await.map_err(|_| PoolError::ActorDead)?;
                rx.await.map_err(|_| PoolError::NoResponse)?
            }
        }
    }

    pub async fn dispatch(
        &self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => handle
                .dispatch(task_id, project_path, model_id)
                .await
                .map_err(PoolError::from),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::Dispatch {
                    task_id: task_id.to_owned(),
                    project_path: project_path.to_owned(),
                    model_id: model_id.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    pub async fn has_session(&self, task_id: &str) -> Result<bool, PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => {
                handle.has_session(task_id).await.map_err(PoolError::from)
            }
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::HasSession {
                    task_id: task_id.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    pub async fn kill_session(&self, task_id: &str) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => {
                handle.kill_session(task_id).await.map_err(PoolError::from)
            }
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::KillSession {
                    task_id: task_id.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    /// Authoritatively terminate an operator/user-requested running session.
    ///
    /// Unlike [`kill_session`], this synchronously reclaims the task mapping,
    /// activity tracker, and running session row before returning. Unlike
    /// [`evict_session`], an unmapped task is reported truthfully as
    /// [`PoolError::TaskNotFound`] rather than treated as an idempotent leak
    /// cleanup no-op.
    pub async fn terminate_session(&self, task_id: &str) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => handle
                .terminate_session(task_id)
                .await
                .map_err(PoolError::from),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::TerminateSession {
                    task_id: task_id.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    /// Forcibly evict a leaked task→slot mapping whose `Killed`/`Free` event
    /// never arrived (dead/evicted/OOM-killed pod, stuck RPC stream). Unlike
    /// [`kill_session`], this does not depend on the pod responding — it
    /// reclaims the in-memory task mapping so the task can redispatch while the
    /// slot itself rejoins rotation only after a later lifecycle event.
    pub async fn evict_session(&self, task_id: &str) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => {
                handle.evict_session(task_id).await.map_err(PoolError::from)
            }
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::EvictSession {
                    task_id: task_id.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    pub async fn pause_session(&self, task_id: &str) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => {
                handle.pause_session(task_id).await.map_err(PoolError::from)
            }
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::PauseSession {
                    task_id: task_id.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    pub async fn get_status(&self) -> Result<PoolStatus, PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => handle
                .get_status()
                .await
                .map(PoolStatus::from)
                .map_err(PoolError::from),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::GetStatus { respond_to: tx })
                    .await
            }
        }
    }

    pub async fn snapshot(&self) -> Result<Vec<DebugSlot>, PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => handle.snapshot().await.map_err(PoolError::from),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::Snapshot { respond_to: tx })
                    .await
            }
        }
    }

    pub async fn session_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<RunningTaskInfo>, PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => handle
                .session_for_task(task_id)
                .await
                .map(|info| info.map(RunningTaskInfo::from))
                .map_err(PoolError::from),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::GetSessionForTask {
                    task_id: task_id.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    pub async fn reconfigure(&self, config: SlotPoolConfig) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => {
                handle.reconfigure(config).await.map_err(PoolError::from)
            }
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::Reconfigure {
                    config,
                    respond_to: tx,
                })
                .await
            }
        }
    }

    pub async fn interrupt_all(&self, reason: &str) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => {
                handle.interrupt_all(reason).await.map_err(PoolError::from)
            }
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::InterruptAll {
                    reason: reason.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    pub async fn interrupt_project(&self, project_id: &str, reason: &str) -> Result<(), PoolError> {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => handle
                .interrupt_project(project_id, reason)
                .await
                .map_err(PoolError::from),
            #[cfg(any(test, feature = "test-support"))]
            SlotPoolInner::Legacy(_) => {
                self.request(|tx| PoolMessage::InterruptProject {
                    project_id: project_id.to_owned(),
                    reason: reason.to_owned(),
                    respond_to: tx,
                })
                .await
            }
        }
    }

    /// Test-only: inject a live `(token_count, turn_count)` override for a
    /// task so the coordinator's session ceiling logic can observe a runaway
    /// session without a real worker bridging `touch_activity`.
    #[cfg(test)]
    pub async fn test_set_token_override(&self, task_id: &str, token_count: u64, turn_count: u64) {
        match &self.inner {
            SlotPoolInner::Canonical(handle) => {
                handle
                    .test_set_token_override(task_id, token_count, turn_count)
                    .await;
            }
            SlotPoolInner::Legacy(sender) => {
                let _ = sender
                    .send(PoolMessage::TestSetTokenOverride {
                        task_id: task_id.to_owned(),
                        token_count,
                        turn_count,
                    })
                    .await;
            }
        }
    }
}

impl TryFrom<SlotPoolHandle> for djinn_slot::SlotPoolHandle {
    type Error = PoolError;

    fn try_from(handle: SlotPoolHandle) -> Result<Self, Self::Error> {
        handle.try_into_djinn_slot()
    }
}
