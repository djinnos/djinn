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
    /// The run was interrupted by INFRASTRUCTURE — a coordinator deploy/rollout
    /// that killed the worker pod, a startup reap of a run orphaned by that
    /// deploy, or a k8s pod eviction/deletion during a rollout — while the task
    /// was still nonterminal, and NO liveness path (stall/ceiling/no-progress/
    /// zombie/hard-runtime) had already claimed the attempt as a failure. This
    /// is an environmental non-attempt (sibling of
    /// [`TaskRunOutcome::EnvironmentalNonAttempt`](crate) semantics): the
    /// attempt is terminalized so nothing wedges, but it must contribute NO
    /// quality strike, NO dispatch-failure streak, NO cooldown escalation, and
    /// NO `reopen_class` penalty — it is treated as if the attempt never ran.
    /// Classified as [`is_infra`](Self::is_infra) so it is excluded from the
    /// quality/park/intervention counters, and the dispatch reappearance path
    /// treats a task whose latest attempt is `Interrupted` as environmental
    /// rather than a same-role failure. Distinct from `Crashed` (a genuine
    /// application/provider crash, which REMAINS a failure) and `TimedOut`
    /// (stall / hard-runtime, which REMAINS a failure).
    Interrupted,
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
            Self::Interrupted => "interrupted",
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

    /// True if this terminal outcome represents an infrastructure /
    /// provider-attempt failure that should be classified as
    /// [`ReopenClass::Infra`](crate::models::ReopenClass::Infra) and excluded
    /// from quality-strike, intervention, and park escalation counters.
    ///
    /// Covers worker handshake timeouts, provider stalls, spawn failures,
    /// timed-out attempts, and crashed infra attempts. Sourced from the `7w2i`
    /// `task_attempts.outcome` contract; matches the set mapped by
    /// `outcome_to_reopen_class`.
    pub fn is_infra(&self) -> bool {
        matches!(
            self,
            Self::TimedOut | Self::SpawnFailed | Self::Crashed | Self::Interrupted
        )
    }

    /// True if this terminal outcome is an ENVIRONMENTAL interruption — the run
    /// was killed by infrastructure (deploy/rollout/reap/pod-eviction) before
    /// any liveness path judged it a failure, so it must be treated as if the
    /// attempt never ran: no dispatch-failure streak, no cooldown escalation,
    /// no quality/park penalty. Narrower than [`is_infra`](Self::is_infra),
    /// which also covers genuine crashes/timeouts that DO remain failures for
    /// the dispatch reappearance streak.
    pub fn is_environmental_interrupt(&self) -> bool {
        matches!(self, Self::Interrupted)
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
            // Ranked ABOVE the failure outcomes (Crashed 32 / TimedOut 33) so a
            // recorded environmental interruption is never clobbered backward to
            // a failure outcome by a racing terminalizer (forward-only advance):
            // once environmental, it stays environmental.
            Self::Interrupted => 41,
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
            "interrupted" => Ok(Self::Interrupted),
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
    /// Concise error string recorded when a GitHub branch publication
    /// failed after the mirror push succeeded (m116/vy47).  Absent when
    /// no publication failure occurred.
    pub github_publication_error: Option<String>,
    /// Immutable coordinator-incarnation UUID that owns this dispatch attempt
    /// (epic jy7g / migration 131).  NULL for legacy rows created before
    /// dispatch ownership wiring landed; never backfilled.
    pub dispatch_owner_incarnation_id: Option<String>,
    /// Dispatch-group UUID correlating the attempt row with its task run and
    /// sibling attempts of the same logical dispatch (epic jy7g / migration
    /// 131).  NULL for legacy rows; never backfilled.
    pub dispatch_group_id: Option<String>,
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
    /// Guard decision that prevented dispatch, if the attempt was deferred.
    pub guard_decision: Option<String>,
    /// Guard reason category, if the attempt was deferred.
    pub guard_reason: Option<String>,
    /// Checkpoint ref/SHA for resumable attempts.
    pub checkpoint_ref: Option<String>,
    /// Arbitrary JSON summary payload (e.g. `failure_class`, `last_verify`).
    pub summary_json: Option<String>,
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
    /// Concise error string recorded when a GitHub branch publication failed
    /// (m116).  Absent when no publication failure occurred.
    pub github_publication_error: Option<String>,
    pub created_at: String,
    pub submitted_at: Option<String>,
    pub terminal_at: Option<String>,
}

/// Metadata about log-tail capture status, derived from a task attempt row.
///
/// Carries presence detection and error-class classification without
/// exposing the raw `log_tail` text.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogTailMeta {
    /// True when the attempt row has a non-NULL `log_tail` value.
    pub log_tail_present: bool,
    /// Machine-classified error category from infra-death log-tail fetch,
    /// extracted from `summary_json->'infra_death_log_tail'->>'fetch_error_class'`.
    pub log_tail_error_class: Option<String>,
}

impl LogTailMeta {
    /// Derive log-tail metadata from raw attempt columns.
    ///
    /// `log_tail` is inspected only for NULL/non-NULL presence; its text is
    /// never propagated.  `summary_json` is parsed for error-class metadata.
    pub fn from_raw(log_tail: Option<&str>, summary_json: Option<&str>) -> Self {
        Self {
            log_tail_present: log_tail.is_some(),
            log_tail_error_class: Self::extract_error_class(summary_json),
        }
    }

    fn extract_error_class(summary_json: Option<&str>) -> Option<String> {
        let json = summary_json?;
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        v.get("infra_death_log_tail")?
            .get("fetch_error_class")?
            .as_str()
            .map(String::from)
    }
}

/// Arbiter/operator audit ledger row for a single attempt.
///
/// Superset of [`TaskAttemptHistoryRow`] adding `summary_json`, explicit
/// log-tail presence/error-class metadata, and model identity when the
/// lifecycle writer recorded it.  Raw `log_tail` text is never included.
///
/// Suitable for `dispatch_ledger_json` consumers and operator audit surfaces.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAttemptLedgerRow {
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
    /// Concise error string recorded when a GitHub branch publication failed
    /// (m116).  Absent when no publication failure occurred.
    pub github_publication_error: Option<String>,
    pub created_at: String,
    pub submitted_at: Option<String>,
    pub terminal_at: Option<String>,
    /// Raw `summary_json` payload (failure class, infra-death metadata, etc.).
    pub summary_json: Option<String>,
    /// True when a `log_tail` value was captured for this attempt.
    pub log_tail_present: bool,
    /// Machine-classified error category from infra-death log-tail fetch.
    pub log_tail_error_class: Option<String>,
    /// Model used for this attempt, when recorded in `summary_json`.
    pub model: Option<String>,
}

impl TaskAttemptLedgerRow {
    /// Build a ledger row from a full [`TaskAttempt`].
    ///
    /// Extracts log-tail presence and error-class metadata from the raw row
    /// without including the `log_tail` text itself.
    pub fn from_task_attempt(attempt: &TaskAttempt) -> Self {
        let meta =
            LogTailMeta::from_raw(attempt.log_tail.as_deref(), attempt.summary_json.as_deref());
        let model = Self::extract_model(attempt.summary_json.as_deref());
        Self {
            id: attempt.id.clone(),
            task_id: attempt.task_id.clone(),
            role: attempt.role.clone(),
            attempt_seq: attempt.attempt_seq,
            dispatch_key: attempt.dispatch_key.clone(),
            session_id: attempt.session_id.clone(),
            outcome: attempt.outcome.clone(),
            guard_decision: attempt.guard_decision.clone(),
            guard_reason: attempt.guard_reason.clone(),
            summary: attempt.summary.clone(),
            checkpoint_ref: attempt.checkpoint_ref.clone(),
            submit_ref: attempt.submit_ref.clone(),
            pr_url: attempt.pr_url.clone(),
            mirror_head_sha: attempt.mirror_head_sha.clone(),
            github_head_sha: attempt.github_head_sha.clone(),
            github_publication_error: attempt.github_publication_error.clone(),
            created_at: attempt.created_at.clone(),
            submitted_at: attempt.submitted_at.clone(),
            terminal_at: attempt.terminal_at.clone(),
            summary_json: attempt.summary_json.clone(),
            log_tail_present: meta.log_tail_present,
            log_tail_error_class: meta.log_tail_error_class,
            model,
        }
    }

    fn extract_model(summary_json: Option<&str>) -> Option<String> {
        let json = summary_json?;
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        v.get("model")?.as_str().map(String::from)
    }
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
            TaskAttemptOutcome::Interrupted,
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
            TaskAttemptOutcome::Interrupted,
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
            assert!(rank >= prev, "ranks must be non-decreasing after submitted");
            prev = rank;
        }
    }

    #[test]
    fn interrupted_is_environmental_infra_and_terminal() {
        let o = TaskAttemptOutcome::Interrupted;
        assert!(o.is_terminal());
        assert!(!o.is_non_terminal());
        // Environmental interrupts are infra-classified (quality/park exempt)…
        assert!(o.is_infra());
        // …and are the ONLY environmental-interrupt outcome.
        assert!(o.is_environmental_interrupt());
        // Genuine crashes / timeouts remain failures — infra, but NOT
        // environmental interrupts (they still feed the dispatch streak).
        assert!(TaskAttemptOutcome::Crashed.is_infra());
        assert!(!TaskAttemptOutcome::Crashed.is_environmental_interrupt());
        assert!(!TaskAttemptOutcome::TimedOut.is_environmental_interrupt());
        assert!(!TaskAttemptOutcome::SpawnFailed.is_environmental_interrupt());
        // Ranked above the failure outcomes so it is never rolled back to a
        // failure by a racing forward-only terminalizer.
        assert!(o.is_forward_from(TaskAttemptOutcome::Crashed));
        assert!(o.is_forward_from(TaskAttemptOutcome::TimedOut));
        assert!(!TaskAttemptOutcome::Crashed.is_forward_from(o));
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
            github_publication_error: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
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
            guard_decision: None,
            guard_reason: None,
            checkpoint_ref: Some("def456".to_string()),
            summary_json: Some(r#"{"failure_class":"compile_error"}"#.to_string()),
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
            github_publication_error: None,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            submitted_at: Some("2026-01-01T00:30:00.000Z".to_string()),
            terminal_at: Some("2026-01-01T01:00:00.000Z".to_string()),
        };
        let serialized = serde_json::to_string(&history).unwrap();
        let deserialized: TaskAttemptHistoryRow = serde_json::from_str(&serialized).unwrap();
        assert_eq!(history, deserialized);
    }

    #[test]
    fn log_tail_meta_from_raw_present_and_absent() {
        // No log tail, no summary_json.
        let meta = LogTailMeta::from_raw(None, None);
        assert!(!meta.log_tail_present);
        assert!(meta.log_tail_error_class.is_none());

        // Log tail present, no summary_json.
        let meta = LogTailMeta::from_raw(Some("tail text"), None);
        assert!(meta.log_tail_present);
        assert!(meta.log_tail_error_class.is_none());

        // Log tail absent, summary_json with error class.
        let sj = r#"{"infra_death_log_tail":{"fetched":false,"fetch_error_class":"timeout"}}"#;
        let meta = LogTailMeta::from_raw(None, Some(sj));
        assert!(!meta.log_tail_present);
        assert_eq!(meta.log_tail_error_class.as_deref(), Some("timeout"));

        // Both present.
        let meta = LogTailMeta::from_raw(Some("tail"), Some(sj));
        assert!(meta.log_tail_present);
        assert_eq!(meta.log_tail_error_class.as_deref(), Some("timeout"));

        // Summary_json without infra_death_log_tail key.
        let sj2 = r#"{"failure_class":"compile_error"}"#;
        let meta = LogTailMeta::from_raw(Some("tail"), Some(sj2));
        assert!(meta.log_tail_present);
        assert!(meta.log_tail_error_class.is_none());
    }

    #[test]
    fn ledger_row_from_task_attempt_extracts_metadata() {
        let attempt = TaskAttempt {
            id: "ta-1".to_string(),
            task_id: "task-1".to_string(),
            role: "worker".to_string(),
            attempt_seq: 2,
            dispatch_key: "dk-2".to_string(),
            session_id: Some("s-1".to_string()),
            outcome: "crashed".to_string(),
            guard_decision: None,
            guard_reason: None,
            summary: Some("crashed mid-run".to_string()),
            summary_json: Some(r#"{"failure_class":"infra_death","model":"claude-3.5-sonnet","infra_death_log_tail":{"fetched":true,"line_count":42,"fetch_error_class":"partial"}}"#.to_string()),
            log_tail: Some("last 100 lines of log...".to_string()),
            checkpoint_ref: Some("cp-1".to_string()),
            submit_ref: Some("sub-1".to_string()),
            pr_url: Some("https://example.com/pr/1".to_string()),
            mirror_head_sha: Some("mirror-sha".to_string()),
            github_head_sha: Some("github-sha".to_string()),
            github_publication_error: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T01:00:00.000Z".to_string(),
            submitted_at: Some("2026-01-01T00:30:00.000Z".to_string()),
            terminal_at: Some("2026-01-01T01:00:00.000Z".to_string()),
        };

        let ledger = TaskAttemptLedgerRow::from_task_attempt(&attempt);
        assert_eq!(ledger.id, "ta-1");
        assert_eq!(ledger.task_id, "task-1");
        assert_eq!(ledger.role, "worker");
        assert_eq!(ledger.attempt_seq, 2);
        assert_eq!(ledger.outcome, "crashed");
        assert_eq!(ledger.session_id.as_deref(), Some("s-1"));
        assert_eq!(ledger.checkpoint_ref.as_deref(), Some("cp-1"));
        assert_eq!(ledger.submit_ref.as_deref(), Some("sub-1"));
        assert_eq!(ledger.pr_url.as_deref(), Some("https://example.com/pr/1"));
        assert_eq!(ledger.mirror_head_sha.as_deref(), Some("mirror-sha"));
        assert_eq!(ledger.github_head_sha.as_deref(), Some("github-sha"));
        assert_eq!(
            ledger.submitted_at.as_deref(),
            Some("2026-01-01T00:30:00.000Z")
        );
        assert_eq!(
            ledger.terminal_at.as_deref(),
            Some("2026-01-01T01:00:00.000Z")
        );
        // Log-tail metadata extracted.
        assert!(ledger.log_tail_present);
        assert_eq!(ledger.log_tail_error_class.as_deref(), Some("partial"));
        assert_eq!(ledger.model.as_deref(), Some("claude-3.5-sonnet"));
        // summary_json is preserved.
        assert!(
            ledger
                .summary_json
                .as_ref()
                .unwrap()
                .contains("failure_class")
        );
    }

    #[test]
    fn ledger_row_serializes_without_log_tail_text() {
        let attempt = TaskAttempt {
            id: "ta-2".to_string(),
            task_id: "task-2".to_string(),
            role: "guard".to_string(),
            attempt_seq: 1,
            dispatch_key: "dk-guard".to_string(),
            session_id: None,
            outcome: "deferred".to_string(),
            guard_decision: Some("defer".to_string()),
            guard_reason: Some("park_rung".to_string()),
            summary: Some("parked".to_string()),
            summary_json: None,
            log_tail: None,
            checkpoint_ref: None,
            submit_ref: None,
            pr_url: None,
            mirror_head_sha: None,
            github_head_sha: None,
            github_publication_error: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
            submitted_at: None,
            terminal_at: Some("2026-01-01T00:00:00.000Z".to_string()),
        };

        let ledger = TaskAttemptLedgerRow::from_task_attempt(&attempt);
        assert!(!ledger.log_tail_present);
        assert!(ledger.log_tail_error_class.is_none());
        assert!(ledger.model.is_none());
        assert_eq!(ledger.guard_decision.as_deref(), Some("defer"));
        assert_eq!(ledger.guard_reason.as_deref(), Some("park_rung"));
        let json = serde_json::to_string(&ledger).unwrap();
        assert!(json.contains("\"log_tail_present\":false"));
        assert!(json.contains("\"log_tail_error_class\":null"));
        assert!(json.contains("\"model\":null"));
        // Round-trip.
        let deserialized: TaskAttemptLedgerRow = serde_json::from_str(&json).unwrap();
        assert_eq!(ledger, deserialized);
    }
}
