//! Coordinator-side orchestration DTOs: debug snapshots and the shared
//! `BackgroundWorkTracker`.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;

pub use djinn_provider::catalog::health::BreakerDebugEntry;

// ─── Debug DTOs ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoordinatorDebugSnapshot {
    pub cooldowns: Vec<DebugCooldown>,
    pub failure_streaks: Vec<DebugFailureStreak>,
    pub inflight_ledger: Vec<DebugInflightEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugCooldown {
    pub task_id: String,
    pub short_id: String,
    pub expires_at: String,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugFailureStreak {
    pub task_id: String,
    pub short_id: String,
    pub streak: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugInflightEntry {
    pub task_id: String,
    pub short_id: String,
    pub creator: Option<String>,
    pub model: String,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugSlot {
    pub slot_id: u32,
    pub model: String,
    pub state: String,
    pub task_id: Option<String>,
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DispatchPauseView {
    pub global: bool,
    pub projects: Vec<String>,
    pub users: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugDispatchState {
    pub snapshot_at: String,
    pub cooldowns: Vec<DebugCooldown>,
    pub failure_streaks: Vec<DebugFailureStreak>,
    pub inflight_ledger: Vec<DebugInflightEntry>,
    pub slot_pool: Vec<DebugSlot>,
    pub breaker: Vec<BreakerDebugEntry>,
    pub paused: DispatchPauseView,
    pub totals: DebugTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugTotals {
    pub cooldowns_active: usize,
    pub inflight_ledger_size: usize,
    pub free_slots: usize,
    pub busy_slots: usize,
    pub open_breakers: usize,
}

// ─── Shared tracker ─────────────────────────────────────────────────────────

/// Shared tracker for in-flight post-session background work (merge/transition
/// for non-worker roles, knowledge extraction). The slot teardown registers
/// task IDs here; the coordinator checks it during stuck detection so it can
/// distinguish tasks with live background work from orphans after restart.
pub type BackgroundWorkTracker = Arc<std::sync::Mutex<HashSet<String>>>;

// ─── Event constants ────────────────────────────────────────────────────────

/// Activity-log event type for PR review feedback injected into the
/// coordinator's prompt context.  Used by both slot (helpers) and coordinator
/// (pr_review_handlers) sides.
pub const PR_REVIEW_FEEDBACK_EVENT: &str = "pr_review_feedback";
