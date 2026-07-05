use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// =============================================================================
// Lifecycle constants
// =============================================================================

/// Maximum length of the `summary` VARCHAR column.
pub const TASK_ATTEMPT_SUMMARY_MAX_LEN: usize = 4000;

/// Maximum length of the `log_tail` VARCHAR column.
pub const TASK_ATTEMPT_LOG_TAIL_MAX_LEN: usize = 8000;

/// Maximum length of the `dispatch_key` VARCHAR column.
pub const TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN: usize = 255;

// =============================================================================
// TaskAttemptOutcome
// =============================================================================

/// Outcome of a single task attempt.
///
/// Wire and DB strings are snake_case. Non-terminal outcomes (`Pending`,
/// `Submitted`) identify attempts that are still live. Terminal outcomes close
/// the attempt row and set `terminal_at`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskAttemptOutcome {
    /// Dispatch keyed; session not yet assigned or not yet submitted.
    Pending,
    /// Worker has signalled submission; awaiting terminal resolution.
    Submitted,

    // --- terminal outcomes ---------------------------------------------------
    /// Normal completion with a merged/approved result.
    Completed,
    /// Reopened to a new attempt by reviewer or arbiter decision.
    Reopened,
    /// Worker/session crashed before terminal resolution.
    Crashed,
    /// Hit a wall-clock or liveness deadline without resolution.
    TimedOut,
    /// Cancelled by operator, host, or arbiter before resolution.
    Cancelled,
    /// Loop-guard tripped (e.g. identical-turn or rework-loop threshold).
    LoopGuardTripped,
    /// Supervisor failed to spawn the worker/session.
    SpawnFailed,
    /// Guard decided to defer the attempt (guard-only row).
    Deferred,
    /// Open PR was adopted rather than creating a fresh attempt.
    AdoptedPr,
    /// Force-closed by host/operator without normal completion.
    ForceClosed,
    /// Handed off to another task, epic, or human process.
    Handoff,
}

impl TaskAttemptOutcome {
    /// Returns the snake_case DB/wire representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitted => "submitted",
            Self::Completed => "completed",
            Self::Reopened => "reopened",
            Self::Crashed => "crashed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::LoopGuardTripped => "loop_guard_tripped",
            Self::SpawnFailed => "spawn_failed",
            Self::Deferred => "deferred",
            Self::AdoptedPr => "adopted_pr",
            Self::ForceClosed => "force_closed",
            Self::Handoff => "handoff",
        }
    }

    /// True if the attempt has reached a terminal outcome.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending | Self::Submitted)
    }

    /// True if the attempt is still non-terminal (`pending` or `submitted`).
    pub fn is_non_terminal(&self) -> bool {
        matches!(self, Self::Pending | Self::Submitted)
    }

    /// Lifecycle rank used for forward-only ordering.
    ///
    /// Non-terminal outcomes are ordered before terminal outcomes. Within each
    /// group the numeric values are stable and increasing; callers can use
    /// `rank()` to enforce "no terminal → non-terminal" rollback.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Pending => 10,
            Self::Submitted => 20,
            Self::Completed => 30,
            Self::Reopened => 31,
            Self::Crashed => 32,
            Self::TimedOut => 33,
            Self::Cancelled => 34,
            Self::LoopGuardTripped => 35,
            Self::SpawnFailed => 36,
            Self::Deferred => 37,
            Self::AdoptedPr => 38,
            Self::ForceClosed => 39,
            Self::Handoff => 40,
        }
    }

    /// True if `self` is a lifecycle advancement over `other`.
    ///
    /// A move is forward when `self.rank() > other.rank()`. Equal ranks are
    /// allowed for idempotent updates. Terminal-to-nonterminal moves are
    /// rejected because terminal ranks are strictly greater than non-terminal
    /// ranks.
    pub fn is_forward_from(&self, other: TaskAttemptOutcome) -> bool {
        self.rank() >= other.rank()
    }
}

impl fmt::Display for TaskAttemptOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskAttemptOutcome {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "submitted" => Ok(Self::Submitted),
            "completed" => Ok(Self::Completed),
            "reopened" => Ok(Self::Reopened),
            "crashed" => Ok(Self::Crashed),
            "timed_out" => Ok(Self::TimedOut),
            "cancelled" => Ok(Self::Cancelled),
            "loop_guard_tripped" => Ok(Self::LoopGuardTripped),
            "spawn_failed" => Ok(Self::SpawnFailed),
            "deferred" => Ok(Self::Deferred),
            "adopted_pr" => Ok(Self::AdoptedPr),
            "force_closed" => Ok(Self::ForceClosed),
            "handoff" => Ok(Self::Handoff),
            other => Err(format!("unknown task_attempt outcome: {other}")),
        }
    }
}

// =============================================================================
// GuardDecision
// =============================================================================

/// Decision returned by a respawn / adoption / defer guard.
///
/// Stored as a short VARCHAR; the string values are chosen so the DB CHECK
/// constraint from migration 94 matches exactly.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GuardDecision {
    /// The guard allows the dispatch to proceed.
    Allow,
    /// The guard defers the dispatch; a `deferred` attempt row may be written.
    Defer,
    /// The guard blocks the dispatch (hard stop, e.g. loop or policy violation).
    Block,
}

impl GuardDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Defer => "defer",
            Self::Block => "block",
        }
    }
}

impl fmt::Display for GuardDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GuardDecision {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(Self::Allow),
            "defer" => Ok(Self::Defer),
            "block" => Ok(Self::Block),
            other => Err(format!("unknown guard decision: {other}")),
        }
    }
}

// =============================================================================
// GuardReason
// =============================================================================

/// Machine-classified reason for a guard decision.
///
/// The variants are stable, string-backed values that map cleanly to the
/// guard_reason free-text column while remaining forward-compatible: new
/// reasons can be added without invalidating existing DB rows.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GuardReason {
    /// No guard reason applies.
    None,
    /// Loop-guard threshold exceeded.
    LoopThreshold,
    /// Identical-turn guard detected a repeated pattern.
    IdenticalTurn,
    /// Respawn guard rejected the attempt.
    RespawnGuard,
    /// Open-PR adoption guard chose to adopt an existing PR.
    OpenPrAdoption,
    /// Park-rung deferral.
    ParkRung,
    /// A dependency is not yet terminal / complete.
    DependencyPending,
    /// Host / operator policy blocked dispatch.
    Policy,
    /// Capacity / slot limit prevented dispatch.
    Capacity,
    /// Transient infra error caused deferral.
    InfraTransient,
}

impl GuardReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LoopThreshold => "loop_threshold",
            Self::IdenticalTurn => "identical_turn",
            Self::RespawnGuard => "respawn_guard",
            Self::OpenPrAdoption => "open_pr_adoption",
            Self::ParkRung => "park_rung",
            Self::DependencyPending => "dependency_pending",
            Self::Policy => "policy",
            Self::Capacity => "capacity",
            Self::InfraTransient => "infra_transient",
        }
    }
}

impl fmt::Display for GuardReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GuardReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "loop_threshold" => Ok(Self::LoopThreshold),
            "identical_turn" => Ok(Self::IdenticalTurn),
            "respawn_guard" => Ok(Self::RespawnGuard),
            "open_pr_adoption" => Ok(Self::OpenPrAdoption),
            "park_rung" => Ok(Self::ParkRung),
            "dependency_pending" => Ok(Self::DependencyPending),
            "policy" => Ok(Self::Policy),
            "capacity" => Ok(Self::Capacity),
            "infra_transient" => Ok(Self::InfraTransient),
            other => Err(format!("unknown guard reason: {other}")),
        }
    }
}

// =============================================================================
// TaskAttempt record
// =============================================================================

/// Persisted record for one dispatch attempt or guard-only deferred decision.
///
/// Mirrors the columns added in migration 94. Optional fields are the same as
/// the SQL NULLable columns. The `summary_json` field is stored as a raw JSON
/// string because `djinn-core` intentionally avoids a SQLx `Json` dependency
/// in the default feature set; repository callers may deserialize it as needed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct TaskAttempt {
    pub id: String,
    pub task_id: String,
    pub role: String,
    pub attempt_seq: i32,
    pub dispatch_key: String,
    pub session_id: Option<String>,
    pub outcome: String,
    pub guard_decision: Option<String>,
    pub guard_reason: Option<String>,
    pub summary: Option<String>,
    pub summary_json: Option<String>,
    pub log_tail: Option<String>,
    pub checkpoint_ref: Option<String>,
    pub submit_ref: Option<String>,
    pub pr_url: Option<String>,
    pub mirror_head_sha: Option<String>,
    pub github_head_sha: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub submitted_at: Option<String>,
    pub terminal_at: Option<String>,
}

impl TaskAttempt {
    /// Convenience accessor for the parsed outcome enum.
    pub fn outcome_enum(&self) -> Result<TaskAttemptOutcome, String> {
        self.outcome.parse()
    }

    /// Convenience accessor for the parsed guard decision enum, if present.
    pub fn guard_decision_enum(&self) -> Result<Option<GuardDecision>, String> {
        match self.guard_decision.as_deref() {
            None => Ok(None),
            Some(s) => s.parse().map(Some),
        }
    }

    /// True when the stored outcome is terminal.
    pub fn is_terminal(&self) -> bool {
        self.outcome_enum()
            .map(|o| o.is_terminal())
            .unwrap_or(false)
    }

    /// True when the stored outcome is non-terminal (`pending` or `submitted`).
    pub fn is_non_terminal(&self) -> bool {
        self.outcome_enum()
            .map(|o| o.is_non_terminal())
            .unwrap_or(false)
    }
}

// =============================================================================
// DTOs for prompt / history consumers
// =============================================================================

/// Bounded summary of a prior attempt for prompt context assembly.
///
/// This is intentionally small and string-based so the coordinator (or any
/// other crate) can include it in prompts without pulling in `djinn-core`
/// implementation details.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAttemptPromptSummary {
    /// Attempt sequence number (1-based).
    pub attempt_seq: i32,
    /// Role of the attempt (e.g. `worker`, `planner`).
    pub role: String,
    /// Terminal outcome of the attempt, or `pending`/`submitted`.
    pub outcome: String,
    /// Human-readable summary, truncated to the DB bound.
    pub summary: Option<String>,
    /// Timestamp when the attempt was created (ISO-8601 UTC).
    pub created_at: String,
    /// Timestamp when the attempt reached a terminal outcome, if any.
    pub terminal_at: Option<String>,
    /// Ref/SHA populated at submission or terminal time.
    pub submit_ref: Option<String>,
    /// PR URL, if the attempt resulted in a PR.
    pub pr_url: Option<String>,
}

/// Arbiter / ledger-facing history row for a single attempt.
///
/// Contains enough information for arbiter decisions, ledger reconciliation,
/// and recovery audit without depending on coordinator internals.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAttemptHistoryRow {
    pub id: String,
    pub task_id: String,
    pub role: String,
    pub attempt_seq: i32,
    pub dispatch_key: String,
    pub session_id: Option<String>,
    pub outcome: String,
    pub guard_decision: Option<String>,
    pub guard_reason: Option<String>,
    pub summary: Option<String>,
    pub checkpoint_ref: Option<String>,
    pub submit_ref: Option<String>,
    pub pr_url: Option<String>,
    pub mirror_head_sha: Option<String>,
    pub github_head_sha: Option<String>,
    pub created_at: String,
    pub submitted_at: Option<String>,
    pub terminal_at: Option<String>,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_as_str_and_round_trip() {
        for outcome in [
            TaskAttemptOutcome::Pending,
            TaskAttemptOutcome::Submitted,
            TaskAttemptOutcome::Completed,
            TaskAttemptOutcome::Reopened,
            TaskAttemptOutcome::Crashed,
            TaskAttemptOutcome::TimedOut,
            TaskAttemptOutcome::Cancelled,
            TaskAttemptOutcome::LoopGuardTripped,
            TaskAttemptOutcome::SpawnFailed,
            TaskAttemptOutcome::Deferred,
            TaskAttemptOutcome::AdoptedPr,
            TaskAttemptOutcome::ForceClosed,
            TaskAttemptOutcome::Handoff,
        ] {
            let s = outcome.as_str();
            let parsed: TaskAttemptOutcome = s.parse().unwrap();
            assert_eq!(parsed, outcome, "round-trip failed for {s}");
            assert_eq!(format!("{outcome}"), s);
        }
    }

    #[test]
    fn outcome_from_str_unknown() {
        assert!("nope".parse::<TaskAttemptOutcome>().is_err());
    }

    #[test]
    fn outcome_terminal_non_terminal() {
        assert!(TaskAttemptOutcome::Pending.is_non_terminal());
        assert!(TaskAttemptOutcome::Submitted.is_non_terminal());
        assert!(!TaskAttemptOutcome::Pending.is_terminal());
        assert!(!TaskAttemptOutcome::Submitted.is_terminal());

        for terminal in [
            TaskAttemptOutcome::Completed,
            TaskAttemptOutcome::Reopened,
            TaskAttemptOutcome::Crashed,
            TaskAttemptOutcome::TimedOut,
            TaskAttemptOutcome::Cancelled,
            TaskAttemptOutcome::LoopGuardTripped,
            TaskAttemptOutcome::SpawnFailed,
            TaskAttemptOutcome::Deferred,
            TaskAttemptOutcome::AdoptedPr,
            TaskAttemptOutcome::ForceClosed,
            TaskAttemptOutcome::Handoff,
        ] {
            assert!(terminal.is_terminal(), "{terminal} should be terminal");
            assert!(
                !terminal.is_non_terminal(),
                "{terminal} should not be non-terminal"
            );
        }
    }

    #[test]
    fn outcome_rank_prevents_terminal_to_nonterminal_rollback() {
        // Non-terminal ranks are strictly less than terminal ranks.
        assert!(TaskAttemptOutcome::Pending.rank() < TaskAttemptOutcome::Completed.rank());
        assert!(TaskAttemptOutcome::Submitted.rank() < TaskAttemptOutcome::Completed.rank());

        // Forward check respects equal rank and advancement.
        assert!(TaskAttemptOutcome::Completed.is_forward_from(TaskAttemptOutcome::Completed));
        assert!(TaskAttemptOutcome::Completed.is_forward_from(TaskAttemptOutcome::Submitted));
        assert!(TaskAttemptOutcome::Submitted.is_forward_from(TaskAttemptOutcome::Pending));

        // Terminal cannot roll back to non-terminal.
        assert!(!TaskAttemptOutcome::Pending.is_forward_from(TaskAttemptOutcome::Completed));
        assert!(!TaskAttemptOutcome::Submitted.is_forward_from(TaskAttemptOutcome::Completed));
    }

    #[test]
    fn outcome_lifecycle_ordering() {
        let outcomes = [
            TaskAttemptOutcome::Pending,
            TaskAttemptOutcome::Submitted,
            TaskAttemptOutcome::Completed,
            TaskAttemptOutcome::Reopened,
            TaskAttemptOutcome::Crashed,
            TaskAttemptOutcome::TimedOut,
            TaskAttemptOutcome::Cancelled,
            TaskAttemptOutcome::LoopGuardTripped,
            TaskAttemptOutcome::SpawnFailed,
            TaskAttemptOutcome::Deferred,
            TaskAttemptOutcome::AdoptedPr,
            TaskAttemptOutcome::ForceClosed,
            TaskAttemptOutcome::Handoff,
        ];
        let mut prev = 0u8;
        for outcome in outcomes {
            let rank = outcome.rank();
            assert!(
                rank > prev || rank == prev,
                "ranks must be non-decreasing after submitted"
            );
            prev = rank;
        }
    }

    #[test]
    fn guard_decision_round_trip() {
        for decision in [
            GuardDecision::Allow,
            GuardDecision::Defer,
            GuardDecision::Block,
        ] {
            let s = decision.as_str();
            assert_eq!(format!("{decision}"), s);
            assert_eq!(s.parse::<GuardDecision>().unwrap(), decision);
        }
        assert!("nope".parse::<GuardDecision>().is_err());
    }

    #[test]
    fn guard_reason_round_trip() {
        for reason in [
            GuardReason::None,
            GuardReason::LoopThreshold,
            GuardReason::IdenticalTurn,
            GuardReason::RespawnGuard,
            GuardReason::OpenPrAdoption,
            GuardReason::ParkRung,
            GuardReason::DependencyPending,
            GuardReason::Policy,
            GuardReason::Capacity,
            GuardReason::InfraTransient,
        ] {
            let s = reason.as_str();
            assert_eq!(format!("{reason}"), s);
            assert_eq!(s.parse::<GuardReason>().unwrap(), reason);
        }
        assert!("nope".parse::<GuardReason>().is_err());
    }

    #[test]
    fn serde_outcomes_snake_case() {
        let outcome = TaskAttemptOutcome::LoopGuardTripped;
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(json, "\"loop_guard_tripped\"");
        let parsed: TaskAttemptOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, outcome);
    }

    #[test]
    fn serde_guard_snake_case() {
        let decision = GuardDecision::Defer;
        let json = serde_json::to_string(&decision).unwrap();
        assert_eq!(json, "\"defer\"");

        let reason = GuardReason::OpenPrAdoption;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, "\"open_pr_adoption\"");
    }

    #[test]
    fn task_attempt_accessors() {
        let attempt = TaskAttempt {
            id: "ta-1".to_string(),
            task_id: "task-1".to_string(),
            role: "worker".to_string(),
            attempt_seq: 1,
            dispatch_key: "dk-1".to_string(),
            session_id: None,
            outcome: "submitted".to_string(),
            guard_decision: None,
            guard_reason: None,
            summary: None,
            summary_json: None,
            log_tail: None,
            checkpoint_ref: None,
            submit_ref: None,
            pr_url: None,
            mirror_head_sha: None,
            github_head_sha: None,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
            submitted_at: None,
            terminal_at: None,
        };
        assert_eq!(
            attempt.outcome_enum().unwrap(),
            TaskAttemptOutcome::Submitted
        );
        assert!(attempt.is_non_terminal());
        assert!(!attempt.is_terminal());
    }

    #[test]
    fn bound_constants_match_migration() {
        assert_eq!(TASK_ATTEMPT_SUMMARY_MAX_LEN, 4000);
        assert_eq!(TASK_ATTEMPT_LOG_TAIL_MAX_LEN, 8000);
        assert_eq!(TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN, 255);
    }

    #[test]
    fn prompt_summary_and_history_row_structs() {
        // Exercise constructors and basic equality to ensure the DTOs compile.
        let summary = TaskAttemptPromptSummary {
            attempt_seq: 1,
            role: "worker".to_string(),
            outcome: "completed".to_string(),
            summary: Some("done".to_string()),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            terminal_at: Some("2026-01-01T01:00:00.000Z".to_string()),
            submit_ref: Some("abc".to_string()),
            pr_url: Some("https://example.com/pr/1".to_string()),
        };
        let serialized = serde_json::to_string(&summary).unwrap();
        let deserialized: TaskAttemptPromptSummary = serde_json::from_str(&serialized).unwrap();
        assert_eq!(summary, deserialized);

        let history = TaskAttemptHistoryRow {
            id: "ta-1".to_string(),
            task_id: "task-1".to_string(),
            role: "worker".to_string(),
            attempt_seq: 1,
            dispatch_key: "dk-1".to_string(),
            session_id: Some("s-1".to_string()),
            outcome: "completed".to_string(),
            guard_decision: None,
            guard_reason: None,
            summary: Some("done".to_string()),
            checkpoint_ref: Some("ref-1".to_string()),
            submit_ref: Some("sub-1".to_string()),
            pr_url: Some("https://example.com/pr/1".to_string()),
            mirror_head_sha: Some("sha-1".to_string()),
            github_head_sha: Some("sha-2".to_string()),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            submitted_at: Some("2026-01-01T00:30:00.000Z".to_string()),
            terminal_at: Some("2026-01-01T01:00:00.000Z".to_string()),
        };
        let serialized = serde_json::to_string(&history).unwrap();
        let deserialized: TaskAttemptHistoryRow = serde_json::from_str(&serialized).unwrap();
        assert_eq!(history, deserialized);
    }
}
