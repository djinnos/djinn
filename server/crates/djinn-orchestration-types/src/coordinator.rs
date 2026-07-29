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

/// The build-admission controller as `/debug/dispatch-state` reports it.
///
/// # Why this block is on the dispatch-state endpoint
///
/// On 2026-07-29 the build-admission controller latched `CreateUnknownHealth`
/// and denied every dispatch on the board for five hours with
/// `cause: "controller_not_admitting"`. `/debug/dispatch-state` — the endpoint
/// whose entire purpose is answering "why is nothing dispatching" — omitted the
/// controller completely, even though `AppState.inner.build_admission` is
/// same-crate. Cooldowns were empty, the slot pool was idle, no breaker was
/// open, dispatch was not paused: every field on the endpoint said the system
/// was healthy, and none of them was the gate that was closed.
///
/// The controller's readiness lives in process-local atomics on the leader, so
/// it is unreachable from any durable query. This endpoint is the only surface
/// that can show it without shelling onto a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugBuildAdmission {
    /// The bounded readiness reason `admit()` gates on: `healthy`,
    /// `create_unknown_health`, `inventory_pending`, `topology_pending`,
    /// `journal_recovery_incomplete`, `journal_unhealthy`,
    /// `seeded_occupancy_above_cap`, `shutdown_draining`.
    pub readiness: String,
    /// True only when `readiness == "healthy"`.
    pub is_ready: bool,
    /// Every currently-failing gate, not just the highest-priority one, so
    /// clearing the first does not leave a second invisible.
    pub unsatisfied_gates: Vec<String>,
    /// `off` / `observe` / `enforce`. Only `enforce` turns a failing gate into
    /// a denial.
    pub mode: String,
    /// The cap actually in force, resolved from the capacity authority.
    pub effective_cap: i64,
    /// The constructor's fallback cap. A disagreement with `effective_cap`
    /// means the durable epoch resolved a different number.
    pub configured_cap: i64,
    /// Build slots in use, or `null` when no capacity authority is installed
    /// or it could not be read. `null` is deliberately not `0`: a readiness
    /// denial never measures occupancy, and printing a fabricated zero is what
    /// made a wedged controller look like a full pool for forty minutes.
    pub occupancy: Option<i64>,
    /// Recovered `create_unknown` rows holding the readiness gate closed.
    pub create_unknown_pending: u64,
    /// WHICH rows are holding it closed, bounded. During the outage an
    /// operator could see `create_unknown=1` and had no way short of raw SQL
    /// against the production database to learn which row it was.
    pub blocking_identities: Vec<String>,
    /// Identities elided by the bound above.
    pub blocking_identities_elided: usize,
    /// Seconds since the last blocker-free reconciliation pass. `null` means
    /// no pass has EVER completed in this process, which is louder than a
    /// large age, not quieter.
    pub seconds_since_last_reconcile: Option<i64>,
    /// This process's admission epoch, for comparison against
    /// `admission_journal.creator_server_epoch`.
    pub server_epoch: String,
    /// Requests parked in the queued-lifecycle map.
    pub queued: usize,
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
    /// `None` only when admission is `Off` and no controller was constructed.
    /// Serialized as `null` rather than omitted, so its absence is a visible
    /// statement rather than a missing key.
    pub build_admission: Option<DebugBuildAdmission>,
    pub totals: DebugTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugTotals {
    pub cooldowns_active: usize,
    pub inflight_ledger_size: usize,
    pub free_slots: usize,
    pub busy_slots: usize,
    pub open_breakers: usize,
    /// True when the build-admission controller is enforcing and NOT ready —
    /// the one-field answer to "is the board wedged behind admission". A
    /// reader who scans only `totals` must not be able to miss this.
    pub build_admission_denying_all: bool,
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
