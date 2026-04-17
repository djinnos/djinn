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

// ─── Shared metadata structs ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeConflictMetadata {
    pub(crate) conflicting_files: Vec<String>,
    pub(crate) base_branch: String,
    pub(crate) merge_target: String,
}

// ─── Submodules ───────────────────────────────────────────────────────────────

mod actor;
mod commands;
pub(crate) mod finalize_handlers;
pub mod helpers;
// Task #8: `lifecycle` is now a thin module owning only the per-stage helpers
// (setup / model / mcp / prompt-context / teardown / retry) reused by the
// supervisor's `execute_stage`.  The legacy `run_task_lifecycle` entry point
// and worktree orchestration have been deleted.
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

pub use actor::*;
pub(crate) use commands::*;
pub use helpers::*;
pub use pool::*;

#[cfg(test)]
mod helpers_tests;

#[cfg(test)]
mod llm_extraction_tests;
