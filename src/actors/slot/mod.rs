use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;

// ─── Slot types ──────────────────────────────────────────────────────────────

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

// ─── Constants ───────────────────────────────────────────────────────────────

pub(crate) const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
pub(crate) const MERGE_VALIDATION_PREFIX: &str = "merge_validation_failed:";

// ─── Shared metadata structs ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeConflictMetadata {
    pub(crate) conflicting_files: Vec<String>,
    pub(crate) base_branch: String,
    pub(crate) merge_target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeValidationFailureMetadata {
    pub(crate) base_branch: String,
    pub(crate) merge_target: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl MergeValidationFailureMetadata {
    pub(crate) fn as_prompt_context(&self) -> String {
        format!(
            "Post-review merge validation failed. Fix the underlying issue, rerun verification, and commit the fix.\n\nmerge_base_branch: {}\nmerge_target_branch: {}\ncommand: git {}\nexit_code: {}\ncwd: {}\nstdout:\n{}\nstderr:\n{}",
            self.base_branch,
            self.merge_target,
            self.command,
            self.exit_code,
            self.cwd,
            self.stdout,
            self.stderr,
        )
    }
}

// ─── Submodules ───────────────────────────────────────────────────────────────

mod actor;
mod commands;
mod conversation_store;
pub(crate) mod task_review;
mod helpers;
mod lifecycle;
mod pool;
mod reply_loop;
mod verification;
mod worktree;

pub use actor::*;
pub(crate) use commands::*;
pub(crate) use task_review::*;
pub(crate) use helpers::*;
pub use lifecycle::run_task_lifecycle;
pub use pool::*;
pub(crate) use worktree::*;
