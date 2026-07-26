//! Slot-side orchestration DTOs: slot info, state, and pool configuration.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

// ─── Slot types ──────────────────────────────────────────────────────────────

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

// ─── Constants ──────────────────────────────────────────────────────────────

pub const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";

// ─── Shared metadata structs ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflictMetadata {
    pub conflicting_files: Vec<String>,
    pub base_branch: String,
    pub merge_target: String,
}
