use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;
use djinn_orchestration_types::coordinator::DebugSlot;

use super::super::SlotPoolConfig;
use super::actor::SlotPool;
#[cfg(any(test, feature = "test-support"))]
use super::types::SlotFactory;
use super::types::{PoolError, PoolMessage, PoolStatus, Reply, RunningTaskInfo};

/// Upper bound on how long a single coordinator→pool ask may block on the
/// pool actor's mailbox + reply before the caller gives up with
/// [`PoolError::Timeout`].
///
/// The slot pool is a single-mailbox actor: every ask is serviced serially by
/// one `select!` loop. On 2026-07-09 an un-timed coordinator→pool ask (tick 72)
/// wedged the *coordinator's* own single-mailbox loop for 11 minutes when the
/// pool was transiently stalled during a session-exit→teardown→redispatch
/// window — starving dispatch, the PR poller, reviewer dispatch, and refinement
/// driving all at once (whole-board freeze, restart-only recovery). Bounding
/// the ask converts that hang into a fast, tolerated `PoolError` on the caller
/// side. The value is comfortably longer than any healthy pool handler (which
/// are in-memory map operations plus at most one short DB read) yet far below
/// the multi-minute freeze it prevents.
pub(crate) const POOL_ASK_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone)]
pub struct SlotPoolHandle {
    sender: mpsc::Sender<PoolMessage>,
}

impl SlotPoolHandle {
    /// Wrap a pre-existing `mpsc::Sender<PoolMessage>` as a `SlotPoolHandle`.
    ///
    /// This is the interop seam used by `djinn-agent`'s coordinator facade
    /// to bridge between the agent-side slot pool (which spawns with
    /// `AgentContext`) and the coordinator (which expects a
    /// `djinn_slot::SlotPoolHandle`).  Both handle types are thin wrappers
    /// around the same `mpsc::Sender<PoolMessage>` channel; the receiving
    /// actor is the same regardless of which crate spawned it.
    ///
    /// # Safety note
    ///
    /// The caller must ensure the sender was created from a slot-pool actor
    /// whose message type is layout-compatible with this crate's
    /// `PoolMessage`.  In practice this holds because both definitions are
    /// kept in sync (same variants, same order, same field types).
    pub fn from_raw_sender(sender: mpsc::Sender<PoolMessage>) -> Self {
        Self { sender }
    }
    pub fn spawn(
        app_state: SlotContext,
        cancel: CancellationToken,
        config: SlotPoolConfig,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(SlotPool::new(receiver, app_state, cancel, config).run());
        Self { sender }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_with_factory(
        app_state: SlotContext,
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
        // Bound BOTH the mailbox enqueue and the reply wait under one deadline.
        // A stalled pool actor can back up its bounded mailbox (so `send` blocks
        // on a full channel) *or* accept the message and never reply (so
        // `rx.await` blocks) — either wedges an un-timed caller. `mpsc::send`
        // and `oneshot::recv` are both cancel-safe: if the deadline fires during
        // `send` the message is not enqueued; if it fires during `rx.await` the
        // message was delivered and the pool will still process it (its reply is
        // simply dropped, which is harmless for the read/idempotent asks).
        match tokio::time::timeout(POOL_ASK_TIMEOUT, async {
            self.sender
                .send(f(tx))
                .await
                .map_err(|_| PoolError::ActorDead)?;
            rx.await.map_err(|_| PoolError::NoResponse)?
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(PoolError::Timeout {
                timeout_secs: POOL_ASK_TIMEOUT.as_secs(),
            }),
        }
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
    /// Additive re-dispatch entry point that threads an optional
    /// resume-via-git lifecycle metadata blob (a JSON-serialized
    /// `djinn_runtime::ResumeLifecycleMetadata`) through the slot pipeline
    /// into the host's `dispatch_task_runtime` and onto
    /// `TaskRunSpec::resume_lifecycle_metadata`. `None` is the
    /// default-off signal: when `worker_lifecycle_config.resume.enabled`
    /// is false (or the coordinator's selector returned no selection),
    /// the helper threads `None` so the existing default dispatch
    /// behavior is byte-for-byte preserved.
    ///
    /// The blob is opaque to the slot pool — it is only forwarded onto
    /// the slot pipeline and decoded by the host. This is the smallest
    /// additive seam that lets the coordinator's
    /// `select_resume_lifecycle_metadata_for_dispatch` reach
    /// `TaskRunSpec` without coupling the slot pool to the coordinator
    /// crate.
    pub async fn dispatch_with_resume_metadata(
        &self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::DispatchWithResume {
            task_id: task_id.to_owned(),
            project_path: project_path.to_owned(),
            model_id: model_id.to_owned(),
            resume_lifecycle_metadata,
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
    /// Authoritatively terminate an operator/user-requested running session.
    ///
    /// Unlike [`kill_session`], this synchronously reclaims the task mapping,
    /// activity tracker, and running session row before returning. Unlike
    /// [`evict_session`], an unmapped task is reported truthfully as
    /// [`PoolError::TaskNotFound`] rather than treated as an idempotent leak
    /// cleanup no-op.
    pub async fn terminate_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::TerminateSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }
    /// Forcibly evict a leaked task→slot mapping whose `Killed`/`Free` event
    /// never arrived (dead/evicted/OOM-killed pod, stuck RPC stream). Unlike
    /// [`kill_session`], this does not depend on the pod responding — it
    /// reclaims the in-memory task mapping so the task can redispatch while the
    /// slot itself rejoins rotation only after a later lifecycle event.
    pub async fn evict_session(&self, task_id: &str) -> Result<(), PoolError> {
        self.request(|tx| PoolMessage::EvictSession {
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
    pub async fn snapshot(&self) -> Result<Vec<DebugSlot>, PoolError> {
        self.request(|tx| PoolMessage::Snapshot { respond_to: tx })
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
    /// Test-only: inject a live `(token_count, turn_count)` override for a
    /// task so the coordinator's session ceiling logic can observe a runaway
    /// session without a real worker bridging `touch_activity`.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn test_set_token_override(&self, task_id: &str, token_count: u64, turn_count: u64) {
        // Fire-and-forget — no reply channel.
        let _ = self
            .sender
            .send(PoolMessage::TestSetTokenOverride {
                task_id: task_id.to_owned(),
                token_count,
                turn_count,
            })
            .await;
    }
}

#[cfg(test)]
#[path = "handle_timeout_tests.rs"]
mod handle_timeout_tests;
