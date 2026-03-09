use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot};

use crate::agent::SessionManager;
use crate::server::AppState;

use super::super::{SlotHandle, SlotPoolConfig};

pub type SlotFactory = Arc<
    dyn Fn(
            usize,
            String,
            mpsc::Sender<super::super::SlotEvent>,
            AppState,
            Arc<SessionManager>,
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
    #[error("failed to load Goose session: {0}")]
    LoadSession(String),
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
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub active_slots: usize,
    pub total_slots: usize,
    pub per_model: HashMap<String, ModelPoolStatus>,
    pub running_tasks: Vec<RunningTaskInfo>,
}

pub type Reply<T> = oneshot::Sender<Result<T, PoolError>>;

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
    PauseSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    GetStatus {
        respond_to: Reply<PoolStatus>,
    },
    GetSessionForTask {
        task_id: String,
        respond_to: Reply<Option<RunningTaskInfo>>,
    },
    GetGooseSession {
        goose_session_id: String,
        respond_to: Reply<goose::session::Session>,
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
}

pub fn now_unix_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}
