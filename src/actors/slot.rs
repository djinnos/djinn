use std::collections::{HashMap, HashSet};

use serde::Serialize;
use thiserror::Error;
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub enum SlotEvent {
    /// Slot finished its task (success or failure) and is free for reassignment.
    Free {
        slot_id: usize,
        model_id: String,
        task_id: String,
    },
    /// Slot's task was killed by external request.
    Killed {
        slot_id: usize,
        model_id: String,
        task_id: String,
    },
}

#[derive(Debug)]
pub enum SlotCommand {
    /// Run a task lifecycle in this slot.
    RunTask {
        task_id: String,
        project_path: String,
        respond_to: oneshot::Sender<Result<(), SlotError>>,
    },
    /// Kill the currently running task.
    Kill,
    /// Pause the currently running task (commit WIP, preserve worktree).
    Pause,
    /// Finish current task then shut down (for capacity reduction).
    Drain,
}

#[derive(Debug, Error, Clone)]
pub enum SlotError {
    #[error("slot is busy")]
    SlotBusy,
    #[error("session failed: {0}")]
    SessionFailed(String),
    #[error("setup failed: {0}")]
    SetupFailed(String),
    #[error("worktree failed: {0}")]
    WorktreeFailed(String),
    #[error("goose error: {0}")]
    GooseError(String),
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotInfo {
    pub slot_id: usize,
    pub model_id: String,
    pub state: SlotState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SlotState {
    Free,
    Busy {
        task_id: String,
        started_at: String,
        agent_type: String,
    },
    Draining,
}

#[derive(Debug, Clone)]
pub struct ModelSlotConfig {
    pub model_id: String,
    pub max_slots: u32,
    pub roles: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct SlotPoolConfig {
    pub models: Vec<ModelSlotConfig>,
    pub role_priorities: HashMap<String, Vec<String>>,
}
