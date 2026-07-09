use std::collections::HashMap;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use tokio::sync::{mpsc, oneshot};

use crate::host::SlotContext;
use djinn_core::clock::Clock;
use djinn_orchestration_types::coordinator::DebugSlot;

use super::super::{SlotHandle, SlotPoolConfig};

pub type SlotFactory = Arc<
    dyn Fn(
            usize,
            String,
            mpsc::Sender<super::super::SlotEvent>,
            SlotContext,
            tokio_util::sync::CancellationToken,
        ) -> SlotHandle
        + Send
        + Sync,
>;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
    /// The coordinator→pool ask exceeded [`super::handle::POOL_ASK_TIMEOUT`]
    /// without the single-mailbox pool actor replying. Introduced after the
    /// 2026-07-09 whole-board freeze: an un-timed coordinator→pool ask on
    /// tick 72 blocked the coordinator's single `select!` loop for 11 minutes
    /// while the pool was transiently stalled in a
    /// session-exit→teardown→redispatch window. Callers already tolerate
    /// `PoolError`, so `Timeout` degrades the same way (ledger preserved,
    /// session treated as not-running, dispatch retried next tick) rather than
    /// wedging the whole coordinator.
    #[error("pool ask timed out after {timeout_secs}s (pool actor stalled)")]
    Timeout { timeout_secs: u64 },
    #[error("task {task_id} already has an active slot")]
    SessionAlreadyActive { task_id: String },
    #[error("task {task_id} has no active slot")]
    TaskNotFound { task_id: String },
    #[error("model {model_id} at capacity")]
    AtCapacity { model_id: String },
    #[error("slot {slot_id} not found")]
    SlotNotFound { slot_id: usize },
    #[error("slot error: {0}")]
    Slot(#[from] super::super::SlotError),
}

#[derive(Debug, Clone)]
pub struct ModelPoolStatus {
    pub active: u32,
    pub free: u32,
    pub total: u32,
}

#[derive(Debug, Clone)]
pub struct RunningTaskInfo {
    pub task_id: String,
    pub model_id: String,
    pub slot_id: usize,
    pub duration_seconds: u64,
    /// Seconds since the session last produced a stream event or completed a
    /// tool call.  Used by stall detection to kill idle sessions.
    pub idle_seconds: u64,
    /// Whether the host's `ActivityTracker` actually has an entry for this task
    /// (i.e. the session has shown at least one sign of life — registered
    /// in-process, or bridged its first `touch_activity` from a remote worker).
    /// `false` means `idle_seconds` is a wall-clock-since-start *fallback*, not a
    /// real idle measurement: the session is still on its very first LLM call.
    /// Stall detection uses this to tell a genuinely-hung first call (aggressive
    /// cap) from a productive session that merely went quiet (full role budget).
    pub activity_tracked: bool,
    /// Project UUID the task belongs to, tracked in the pool so project-scoped
    /// queries can filter running tasks without depending on a DB session row
    /// (which does not exist during pre-session lifecycle stages).
    pub project_id: Option<String>,
    /// Live token spend for this session, sourced from the worker's
    /// `touch_activity` RPC (not the DB row, which is only flushed at session
    /// end).  Used by the coordinator's per-session token ceiling to catch
    /// runaway loops before they consume unbounded resources.
    pub token_count: u64,
    /// Live turn count for this session, sourced from the worker's
    /// `touch_activity` RPC.  Used by the coordinator's per-session turn
    /// ceiling to detect structurally-stuck sessions.
    pub turn_count: u64,
    /// Live no-progress streak for this session, sourced from the worker's
    /// durable-progress detector observations (y56l/yttk).  When the worker
    /// reports consecutive evaluated turns without durable progress, this
    /// counter increments; it resets on a durable-progress observation.
    /// Used by the coordinator's no-progress lifecycle enforcement (y8pv).
    /// Defaults to 0 when the worker has not reported streak data (pre-yttk
    /// workers or shadow-only mode).
    pub no_progress_streak: u32,
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub active_slots: usize,
    pub total_slots: usize,
    pub per_model: HashMap<String, ModelPoolStatus>,
    pub running_tasks: Vec<RunningTaskInfo>,
}

pub(super) type Reply<T> = oneshot::Sender<Result<T, PoolError>>;

pub enum PoolMessage {
    Dispatch {
        task_id: String,
        project_path: String,
        model_id: String,
        respond_to: Reply<()>,
    },
    /// Additive variant for re-dispatch with optional resume-via-git
    /// lifecycle metadata. Carries the same fields as [`PoolMessage::Dispatch`]
    /// plus a JSON-serialized `ResumeLifecycleMetadata` blob (or `None` for
    /// disabled/default behavior — that is the canonical "no resume selection
    /// was made" signal, equivalent to the legacy `Dispatch` path). Selected
    /// resume metadata rides the slot pipeline unchanged into
    /// `TaskRunSpec::resume_lifecycle_metadata`; downstream prompt/model/
    /// merge work (`48ru`/`twsk`/`sy0g`) reads it from there. The legacy
    /// `Dispatch` variant stays byte-compatible so existing callers and the
    /// `#[cfg(any(test, feature = "test-support"))]` injection points keep
    /// compiling without re-plumbing the resume blob.
    DispatchWithResume {
        task_id: String,
        project_path: String,
        model_id: String,
        /// JSON-serialized `djinn_runtime::ResumeLifecycleMetadata` (the
        /// serde-only mirror defined in `djinn_runtime::spec`). `None` means
        /// resume selection was not consulted (default/off path) and is
        /// equivalent to a legacy `Dispatch` call for runtime purposes.
        resume_lifecycle_metadata: Option<serde_json::Value>,
        respond_to: Reply<()>,
    },
    HasSession {
        task_id: String,
        respond_to: Reply<bool>,
    },
    KillSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    TerminateSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    EvictSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    PauseSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    GetStatus {
        respond_to: Reply<PoolStatus>,
    },
    Snapshot {
        respond_to: Reply<Vec<DebugSlot>>,
    },
    GetSessionForTask {
        task_id: String,
        respond_to: Reply<Option<RunningTaskInfo>>,
    },
    Reconfigure {
        config: SlotPoolConfig,
        respond_to: Reply<()>,
    },
    InterruptAll {
        reason: String,
        respond_to: Reply<()>,
    },
    InterruptProject {
        project_id: String,
        reason: String,
        respond_to: Reply<()>,
    },
    /// Test-only: inject a live `(token_count, turn_count)` override for a
    /// task so the coordinator's session ceiling logic can observe a runaway
    /// session without a real worker bridging `touch_activity`. No-op in
    /// non-test builds (the variant is behind `#[cfg(test)]`).
    #[cfg(any(test, feature = "test-support"))]
    TestSetTokenOverride {
        task_id: String,
        token_count: u64,
        turn_count: u64,
    },
}

/// Format the current unix-seconds timestamp (read through the shared clock
/// seam) as a decimal string.  Falls back to `"0"` when the wall-clock is
/// before `UNIX_EPOCH`.
pub(super) fn now_unix_string(clock: &dyn Clock) -> String {
    let secs = clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}
