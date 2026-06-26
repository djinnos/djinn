use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot};

use crate::context::AgentContext;
use djinn_orchestration_types::coordinator::DebugSlot;

use super::super::{SlotHandle, SlotPoolConfig};

pub type SlotFactory = Arc<
    dyn Fn(
            usize,
            String,
            mpsc::Sender<super::super::SlotEvent>,
            AgentContext,
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
    #[cfg(test)]
    TestSetTokenOverride {
        task_id: String,
        token_count: u64,
        turn_count: u64,
    },
}

pub(super) fn now_unix_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}
