use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

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
// Task #7: conversation_store is part of the legacy lifecycle resume path.
// No production caller after the supervisor switch; kept for rollback and
// deleted wholesale in task #8.
#[allow(dead_code)]
mod conversation_store;
pub(crate) mod finalize_handlers;
pub mod helpers;
// Task #7: legacy per-agent lifecycle.  Still exercised by `lifecycle_tests`
// but unreachable from the production dispatch path.
#[allow(dead_code)]
pub(crate) mod lifecycle;
pub(crate) mod llm_extraction;
mod pool;
pub(crate) mod reply_loop;
#[cfg(test)]
mod reply_loop_tests;
pub(crate) mod session_extraction;
mod supervisor_runner;
pub(crate) mod task_review;
pub(crate) mod verification;
pub mod worktree;

pub use actor::*;
pub(crate) use commands::*;
pub use helpers::*;
#[allow(unused_imports)]
pub(crate) use lifecycle::run_task_lifecycle;
pub use pool::*;
pub use worktree::*;

#[cfg(test)]
mod helpers_tests;

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod llm_extraction_tests;

#[cfg(test)]
mod worktree_tests;
