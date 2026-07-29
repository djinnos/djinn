//! Pure liveness classifier for coordinator-side session/task evaluation.
//!
//! This module provides the shared liveness taxonomy and a side-effect-free
//! classifier that combines normalized evidence into structured verdicts. It
//! does **not** mutate tasks, kill pods, extend claims, or replace existing
//! `session_recovery.rs` behavior — it only produces importable types and
//! verdicts for later consumer epics.
//!
//! # Precedence invariants
//!
//! 1. Terminal task state produces a noop/idempotent outcome and wins over
//!    liveness races.
//! 2. Hard runtime cap outranks `Live`/`Slow` and forbids extension.
//! 3. `ProtocolViolation` captures inconsistent clean/nonzero/nonterminal
//!    outcomes structurally.
//! 4. `Dead` requires absent/failed pod evidence plus no recent in-memory or
//!    DB activity.
//! 5. `Slow` below hard cap includes enough evidence for later claim extension
//!    without incrementing attempts.
//!
// Allow dead_code for public API types that are designed for import by later
// consumer epics (reaper integration, board health, doctor diagnostics).
#![allow(dead_code)]

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ─── Pod phase (normalized from K8s or runtime bridge) ──────────────────────

/// Normalized pod lifecycle phase, independent of the K8s API surface.
///
/// Callers map from whatever representation they hold (K8s `PodPhase`, runtime
/// bridge slot state, or `None` for absent) into this enum before calling the
/// classifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PodPhase {
    /// Pod is actively running.
    Running,
    /// Pod terminated successfully (exit code 0).
    Succeeded,
    /// Pod terminated with a non-zero exit code or was evicted/killed.
    Failed,
    /// Pod exists but has not started (pending scheduling, image pull, etc.).
    Pending,
    /// Pod phase is recognized but not mapped to a known variant.
    Unknown,
    /// No pod exists for this session/run (absent from the runtime bridge or
    /// already GC'd).
    Absent,
}

// ─── In-memory activity signal ──────────────────────────────────────────────

/// Normalized in-memory session activity signal from the coordinator's
/// `ActivityTracker` / slot bookkeeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySignal {
    /// Activity was observed within the recency window.
    Active,
    /// No activity recorded, but the session has been observed (tracker has an
    /// entry with an aged timestamp).
    Idle,
    /// No activity tracker entry exists for this session at all (first-call
    /// stall or tracker drift).
    NeverActive,
}

// ─── DB session status (normalized) ─────────────────────────────────────────

/// Normalized DB session status. Maps from the `djinn_core::models::SessionStatus`
/// wire values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbSessionStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
    Paused,
}

impl DbSessionStatus {
    /// Whether this status represents a terminal (finished) session.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

// ─── DB task status (normalized) ────────────────────────────────────────────

/// Normalized DB task status. Maps from the `djinn_core::models::TaskStatus`
/// wire values. The `Closed` variant is the only terminal task state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbTaskStatus {
    Open,
    InProgress,
    NeedsTaskReview,
    InTaskReview,
    Approved,
    PrDraft,
    PrReview,
    NeedsLeadIntervention,
    InLeadIntervention,
    Closed,
}

impl DbTaskStatus {
    /// Whether this status represents a terminal task (Closed).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// Whether this status is a **recorded handoff**: the task has moved to a
    /// boundary that some other actor now owns (review, PR, lead intervention)
    /// or has finished outright.
    ///
    /// This is deliberately wider than [`Self::is_terminal`] and exists only for
    /// protocol-violation detection. A session that exits with its task parked
    /// at a handoff boundary did its job and handed the task on — that is the
    /// normal shape of a worker or reviewer turn, not a structural
    /// inconsistency. Treating every non-`closed` status as "nonterminal" made
    /// the violation branch fire on 74.8% of ALL session exits in production
    /// (2133/2852 over seven days), including 741 clean reviewer exits at
    /// `in_task_review` and 950 handoff exits in total, which drowned the signal
    /// the check exists to raise.
    ///
    /// `Open` and `InProgress` are NOT settled: a pod that exits while the task
    /// is still queued or still claimed left nothing behind, which is the
    /// genuine inconsistency this detector is for.
    ///
    /// Status alone is not the whole test, though. `Open` is ALSO where a
    /// reviewer's rejection parks a task (`TransitionAction::TaskReviewReject`),
    /// so [`classify`] additionally consults
    /// [`LivenessEvidence::handed_off_from_session_held_status`] before
    /// convicting — see the precedence-3 comment there.
    pub fn is_settled(&self) -> bool {
        matches!(
            self,
            Self::NeedsTaskReview
                | Self::InTaskReview
                | Self::Approved
                | Self::PrDraft
                | Self::PrReview
                | Self::NeedsLeadIntervention
                | Self::InLeadIntervention
                | Self::Closed
        )
    }
}

// ─── Classification evidence ────────────────────────────────────────────────

/// Normalized evidence packet fed into the pure classifier.
///
/// Every field is a primitive or small enum — callers are responsible for
/// mapping from their own rich types (K8s pods, `SessionRecord`, `Task`,
/// `Instant`, etc.) into this DTO before calling [`classify`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LivenessEvidence {
    /// Pod lifecycle phase. `None` means no pod/runtime information is
    /// available (unknown or absent — treated as `PodPhase::Absent`).
    pub pod_phase: Option<PodPhase>,
    /// In-memory activity signal from the coordinator's session tracker.
    pub activity: ActivitySignal,
    /// DB session status, if a session row exists.
    pub db_session_status: Option<DbSessionStatus>,
    /// DB task status, if a task row exists.
    pub db_task_status: Option<DbTaskStatus>,
    /// Remaining time until the claim TTL expires. `None` if no claim is held
    /// or TTL tracking is not active.
    pub claim_ttl_remaining: Option<Duration>,
    /// Whether the session has already exhausted its claim-extension budget
    /// (i.e. a slow extension would be rejected).
    pub extension_budget_exhausted: bool,
    /// Absolute deadline for the task run. `None` if no hard runtime cap is
    /// configured. When the current time exceeds this deadline, the classifier
    /// forces a `Dead` verdict with `hard_runtime_exceeded` reason.
    pub hard_runtime_deadline_exceeded: bool,
    /// Exit code of the pod/process, if known. Used to distinguish clean exit
    /// (code 0) from nonzero exit. `None` when the pod is still running or
    /// absent.
    pub exit_code: Option<i32>,
    /// Positive evidence that the task was **handed off** by a live session:
    /// its most recent recorded status transition moved it OUT of a status that
    /// only a running session holds (see [`is_session_held_status`]).
    ///
    /// A task can sit on a non-settled status (`open` / `in_progress`) for two
    /// opposite reasons, and the status alone cannot tell them apart:
    ///
    /// * `in_task_review → open` — a reviewer explicitly rejected and queued the
    ///   task for rework. The reviewer completed its protocol exactly as
    ///   designed; the task is dispatchable and nothing is stranded.
    /// * `open → in_progress` (and then nothing) — a worker claimed the task and
    ///   exited without ever handing it on. The task now claims to be in
    ///   progress with no session behind it: genuinely stranded.
    ///
    /// `true` exonerates: some session deliberately moved the task off an
    /// active status. `false` is the fail-safe default and preserves the
    /// pre-existing verdict.
    #[serde(default)]
    pub handed_off_from_session_held_status: bool,
}

/// Whether `status` is one that only a LIVE session holds.
///
/// These are the statuses whose meaning is "a session is working on this right
/// now". A transition OUT of one of them is a session's deliberate handoff; a
/// transition INTO one is a claim, which proves nothing about how the session
/// ended.
pub fn is_session_held_status(status: &str) -> bool {
    matches!(
        status,
        "in_progress" | "in_task_review" | "in_lead_intervention"
    )
}

// ─── Stable outcome kinds ───────────────────────────────────────────────────

/// Stable machine-readable liveness outcome kind.
///
/// These map to the outcome taxonomy from the epic/proposal and are designed
/// to be persisted as VARCHAR values. They are distinct from the dispatch
/// outcomes in [`DispatchOutcome`](super::outcome::DispatchOutcome).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessOutcome {
    /// Session/task completed normally.
    Success,
    /// Pod/process crashed (non-zero exit or unexpected termination).
    Crash,
    /// Hard runtime deadline exceeded.
    Timeout,
    /// Session was dead and its resources were reclaimed.
    DeadReclaimed,
    /// Protocol-level inconsistency was detected.
    ProtocolViolation,
    /// Kill/reap was attempted but the task was already terminal (noop).
    KillNoop,
    /// Slow session received a claim extension.
    SlowExtended,
}

impl LivenessOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Crash => "crash",
            Self::Timeout => "timeout",
            Self::DeadReclaimed => "dead_reclaimed",
            Self::ProtocolViolation => "protocol_violation",
            Self::KillNoop => "kill_noop",
            Self::SlowExtended => "slow_extended",
        }
    }
}

impl fmt::Display for LivenessOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Stable reason strings ──────────────────────────────────────────────────

/// Machine-readable reason complementing a [`LivenessOutcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessReason {
    /// Session exited cleanly (code 0) but the task is not in a terminal state.
    CleanExitNonterminal,
    /// Session exited with a nonzero code but the task is not in a terminal
    /// state.
    NonzeroExitNonterminal,
    /// The hard runtime cap was exceeded.
    HardRuntimeExceeded,
    /// The claim-extension budget is exhausted — no more slow extensions
    /// available.
    SlowExtensionBudgetExhausted,
    /// No specific reason (e.g. successful completion, live session).
    None,
}

impl LivenessReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CleanExitNonterminal => "clean_exit_nonterminal",
            Self::NonzeroExitNonterminal => "nonzero_exit_nonterminal",
            Self::HardRuntimeExceeded => "hard_runtime_exceeded",
            Self::SlowExtensionBudgetExhausted => "slow_extension_budget_exhausted",
            Self::None => "none",
        }
    }
}

impl fmt::Display for LivenessReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Verdict ────────────────────────────────────────────────────────────────

/// Liveness verdict produced by the classifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Session is alive and progressing normally.
    Live,
    /// Session appears alive but has stalled below the hard runtime cap.
    /// Eligible for claim extension without incrementing attempts.
    Slow,
    /// Session is dead (absent/failed pod with no recent activity). Eligible
    /// for resource reclamation.
    Dead,
    /// Protocol-level inconsistency detected (e.g. clean exit on non-terminal
    /// task, nonzero exit on non-terminal task with no session row).
    ProtocolViolation,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Slow => "slow",
            Self::Dead => "dead",
            Self::ProtocolViolation => "protocol_violation",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Structured classifier result ───────────────────────────────────────────

/// The structured result returned by [`classify`]. Carries the verdict together
/// with the evidence snapshot and an optional stable outcome/reason pair for
/// downstream persistence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassificationResult {
    /// The liveness verdict.
    pub verdict: Verdict,
    /// Echo of the evidence that produced this verdict (for audit/persistence).
    pub evidence: LivenessEvidence,
    /// Stable outcome kind. Present when the verdict implies a concrete action
    /// outcome (e.g. `Dead → DeadReclaimed`, `ProtocolViolation →
    /// ProtocolViolation`). `None` for `Live`/`Slow` verdicts where no action
    /// outcome is determined yet.
    pub outcome: Option<LivenessOutcome>,
    /// Stable reason string complementing `outcome`. `None` when `outcome` is
    /// `None`.
    pub reason: Option<LivenessReason>,
    /// Whether the session is eligible for claim extension (relevant only for
    /// `Slow` verdicts).
    pub extension_eligible: bool,
}

// ─── Pure classifier ────────────────────────────────────────────────────────

/// Pure, side-effect-free liveness classifier.
///
/// Combines normalized evidence into a structured verdict following the
/// precedence invariants documented at module level. This function does NOT
/// mutate tasks, kill pods, extend claims, or write to any external store.
///
/// # Precedence order
///
/// 1. **Terminal task state** → noop/idempotent outcome
/// 2. **Hard runtime cap exceeded** → `Dead` with `Timeout` outcome (forbids
///    extension)
/// 3. **Protocol violation** → inconsistent clean/nonzero exit on a task that
///    is positively known to be unsettled (still `open`/`in_progress`), with a
///    session row positively known to still be running, and with no evidence
///    that a session deliberately handed the task off to that status
/// 4. **Dead** → absent/failed pod with no recent activity
/// 5. **Slow** → activity signal absent/idle, pod running, below hard cap
/// 6. **Live** → default when other conditions don't match
pub fn classify(evidence: &LivenessEvidence) -> ClassificationResult {
    // ── 1. Terminal task state → noop ────────────────────────────────────
    if evidence
        .db_task_status
        .as_ref()
        .is_some_and(|s| s.is_terminal())
    {
        return ClassificationResult {
            verdict: Verdict::Live, // verdict is moot; the outcome is noop
            evidence: evidence.clone(),
            outcome: Some(LivenessOutcome::KillNoop),
            reason: Some(LivenessReason::None),
            extension_eligible: false,
        };
    }

    // ── 2. Hard runtime cap exceeded ────────────────────────────────────
    if evidence.hard_runtime_deadline_exceeded {
        return ClassificationResult {
            verdict: Verdict::Dead,
            evidence: evidence.clone(),
            outcome: Some(LivenessOutcome::Timeout),
            reason: Some(LivenessReason::HardRuntimeExceeded),
            extension_eligible: false,
        };
    }

    // ── 3. Protocol violation detection ─────────────────────────────────
    // A protocol violation is a STRUCTURAL INCONSISTENCY, so every term must be
    // POSITIVE evidence. Absence is not guilt.
    //
    // The previous rule mixed one positive guard with one fail-open one:
    // precedence 1 needed `db_task_status.is_some_and(is_terminal)` to hold to
    // exonerate, while this branch used `db_session_status.is_none_or(...)` to
    // convict. Missing evidence therefore fell through precedence 1 AND
    // satisfied this branch — an absent task row or session row convicted
    // deterministically. Both terms are now `is_some_and`, so an unknown state
    // is fail-safe: it produces no violation, and no destructive action either
    // (the reaping paths are precedence 2/4, which are untouched).
    //
    // The task term is also tightened from "not closed" to "not settled": see
    // [`DbTaskStatus::is_settled`]. A session that exits with its task at a
    // review/PR/intervention boundary handed off; only a task still `open` or
    // `in_progress` was genuinely left stranded.
    let pod_exited = matches!(
        evidence.pod_phase,
        Some(PodPhase::Succeeded) | Some(PodPhase::Failed)
    );
    let session_nonterminal = evidence
        .db_session_status
        .as_ref()
        .is_some_and(|s| !s.is_terminal());
    let task_unsettled = evidence
        .db_task_status
        .as_ref()
        .is_some_and(|s| !s.is_settled());

    // A non-settled status is not by itself evidence of abandonment. The task's
    // own transition record decides: if the last status change moved the task
    // OUT of a session-held status, a session handed it on deliberately
    // (`in_task_review → open` is the reviewer-rejection path, which the
    // supervisor drives on EVERY explicit reject verdict — see
    // `TransitionAction::TaskReviewReject`). Only a task left sitting where its
    // session found it — claimed and unmoved — is genuinely stranded.
    let handed_off = evidence.handed_off_from_session_held_status;

    if pod_exited && session_nonterminal && task_unsettled && !handed_off {
        // Classify the type of protocol violation based on exit code
        let reason = match evidence.exit_code {
            Some(0) => LivenessReason::CleanExitNonterminal,
            Some(_) => LivenessReason::NonzeroExitNonterminal,
            None => {
                // Pod succeeded/failed but exit code unknown — still a
                // violation if the session isn't terminal.
                if evidence.pod_phase == Some(PodPhase::Succeeded) {
                    LivenessReason::CleanExitNonterminal
                } else {
                    LivenessReason::NonzeroExitNonterminal
                }
            }
        };

        let outcome = if evidence.pod_phase == Some(PodPhase::Failed)
            || evidence.exit_code.is_some_and(|c| c != 0)
        {
            LivenessOutcome::Crash
        } else {
            LivenessOutcome::Success
        };

        return ClassificationResult {
            verdict: Verdict::ProtocolViolation,
            evidence: evidence.clone(),
            outcome: Some(outcome),
            reason: Some(reason),
            extension_eligible: false,
        };
    }

    // ── 4. Dead: absent/failed pod + no recent activity ─────────────────
    let pod_absent_or_failed = matches!(
        evidence.pod_phase,
        Some(PodPhase::Absent) | Some(PodPhase::Failed) | None
    );
    let no_recent_activity = matches!(
        evidence.activity,
        ActivitySignal::Idle | ActivitySignal::NeverActive
    );

    if pod_absent_or_failed && no_recent_activity {
        let (outcome, reason) = match evidence.pod_phase {
            Some(PodPhase::Failed) => {
                let r = if evidence.exit_code.is_some_and(|c| c != 0) {
                    LivenessReason::NonzeroExitNonterminal
                } else {
                    LivenessReason::None
                };
                (LivenessOutcome::Crash, r)
            }
            _ => (LivenessOutcome::DeadReclaimed, LivenessReason::None),
        };

        return ClassificationResult {
            verdict: Verdict::Dead,
            evidence: evidence.clone(),
            outcome: Some(outcome),
            reason: if reason == LivenessReason::None {
                None
            } else {
                Some(reason)
            },
            extension_eligible: false,
        };
    }

    // ── 5. Slow: pod running but activity signal absent/idle ────────────
    let pod_running = matches!(evidence.pod_phase, Some(PodPhase::Running));
    if pod_running && no_recent_activity {
        let extension_eligible = !evidence.extension_budget_exhausted;
        let reason = if evidence.extension_budget_exhausted {
            Some(LivenessReason::SlowExtensionBudgetExhausted)
        } else {
            None
        };

        return ClassificationResult {
            verdict: Verdict::Slow,
            evidence: evidence.clone(),
            outcome: if evidence.extension_budget_exhausted {
                Some(LivenessOutcome::SlowExtended)
            } else {
                None
            },
            reason,
            extension_eligible,
        };
    }

    // ── 6. Live: default ────────────────────────────────────────────────
    ClassificationResult {
        verdict: Verdict::Live,
        evidence: evidence.clone(),
        outcome: None,
        reason: None,
        extension_eligible: false,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a minimal "all-green" evidence packet.
    fn live_evidence() -> LivenessEvidence {
        LivenessEvidence {
            pod_phase: Some(PodPhase::Running),
            activity: ActivitySignal::Active,
            db_session_status: Some(DbSessionStatus::Running),
            db_task_status: Some(DbTaskStatus::InProgress),
            claim_ttl_remaining: Some(Duration::from_secs(600)),
            extension_budget_exhausted: false,
            hard_runtime_deadline_exceeded: false,
            exit_code: None,

            handed_off_from_session_held_status: false,
        }
    }

    // ── Precedence 1: terminal task state → noop ─────────────────────────

    #[test]
    fn terminal_task_closed_produces_kill_noop() {
        let mut ev = live_evidence();
        ev.db_task_status = Some(DbTaskStatus::Closed);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Live); // verdict moot
        assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
        assert_eq!(result.reason, Some(LivenessReason::None));
        assert!(!result.extension_eligible);
    }

    #[test]
    fn terminal_task_wins_over_hard_runtime() {
        let mut ev = live_evidence();
        ev.db_task_status = Some(DbTaskStatus::Closed);
        ev.hard_runtime_deadline_exceeded = true;

        let result = classify(&ev);
        assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
        // Terminal task should NOT produce Timeout even though hard cap is set
        assert_ne!(result.outcome, Some(LivenessOutcome::Timeout));
    }

    #[test]
    fn terminal_task_wins_over_dead_signals() {
        let mut ev = live_evidence();
        ev.db_task_status = Some(DbTaskStatus::Closed);
        ev.pod_phase = Some(PodPhase::Absent);
        ev.activity = ActivitySignal::NeverActive;

        let result = classify(&ev);
        assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
    }

    #[test]
    fn terminal_task_wins_over_protocol_violation() {
        let mut ev = live_evidence();
        ev.db_task_status = Some(DbTaskStatus::Closed);
        ev.pod_phase = Some(PodPhase::Succeeded);
        ev.db_session_status = Some(DbSessionStatus::Running); // would be protocol violation if not terminal task
        ev.exit_code = Some(0);

        let result = classify(&ev);
        assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
    }

    // ── Precedence 2: hard runtime cap ───────────────────────────────────

    #[test]
    fn hard_runtime_cap_produces_dead_timeout() {
        let mut ev = live_evidence();
        ev.hard_runtime_deadline_exceeded = true;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Dead);
        assert_eq!(result.outcome, Some(LivenessOutcome::Timeout));
        assert_eq!(result.reason, Some(LivenessReason::HardRuntimeExceeded));
        assert!(!result.extension_eligible);
    }

    #[test]
    fn hard_runtime_cap_forbids_extension_even_when_slow() {
        let mut ev = live_evidence();
        ev.hard_runtime_deadline_exceeded = true;
        ev.activity = ActivitySignal::Idle;
        ev.extension_budget_exhausted = false;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Dead);
        assert!(!result.extension_eligible);
    }

    // ── Precedence 3: protocol violation ─────────────────────────────────

    #[test]
    fn clean_exit_on_nonterminal_task_is_protocol_violation() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Succeeded);
        ev.exit_code = Some(0);
        // Task not terminal, session still running
        ev.db_task_status = Some(DbTaskStatus::InProgress);
        ev.db_session_status = Some(DbSessionStatus::Running);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert_eq!(result.outcome, Some(LivenessOutcome::Success));
        assert_eq!(result.reason, Some(LivenessReason::CleanExitNonterminal));
    }

    #[test]
    fn nonzero_exit_on_nonterminal_task_is_protocol_violation() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Failed);
        ev.exit_code = Some(137);
        ev.db_task_status = Some(DbTaskStatus::InProgress);
        ev.db_session_status = Some(DbSessionStatus::Running);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
        assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
    }

    #[test]
    fn succeeded_pod_unknown_exit_still_clean_violation() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Succeeded);
        ev.exit_code = None; // unknown
        ev.db_task_status = Some(DbTaskStatus::InProgress);
        ev.db_session_status = Some(DbSessionStatus::Running);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert_eq!(result.reason, Some(LivenessReason::CleanExitNonterminal));
    }

    #[test]
    fn terminal_session_with_exited_pod_is_not_protocol_violation() {
        // If the session is already terminal, the pod exit is expected
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Succeeded);
        ev.exit_code = Some(0);
        ev.db_session_status = Some(DbSessionStatus::Completed);
        ev.activity = ActivitySignal::Active;

        let result = classify(&ev);
        assert_ne!(result.verdict, Verdict::ProtocolViolation);
    }

    // ── Precedence 4: Dead ───────────────────────────────────────────────

    #[test]
    fn absent_pod_with_no_activity_is_dead() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Absent);
        ev.activity = ActivitySignal::Idle;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Dead);
        assert_eq!(result.outcome, Some(LivenessOutcome::DeadReclaimed));
    }

    #[test]
    fn failed_pod_with_terminal_session_and_no_activity_is_dead() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Failed);
        ev.exit_code = Some(1);
        ev.activity = ActivitySignal::Idle;
        // Session already terminated — pod exit is expected, not a violation
        ev.db_session_status = Some(DbSessionStatus::Failed);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Dead);
        assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
        assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
    }

    #[test]
    fn failed_pod_with_running_session_is_protocol_violation() {
        // A Failed pod while the DB session is still Running is structurally
        // inconsistent — the protocol-violation check fires before Dead.
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Failed);
        ev.exit_code = Some(137);
        ev.activity = ActivitySignal::Idle;
        ev.db_session_status = Some(DbSessionStatus::Running);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
        assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
    }

    #[test]
    fn no_pod_phase_with_never_active_is_dead() {
        let mut ev = live_evidence();
        ev.pod_phase = None;
        ev.activity = ActivitySignal::NeverActive;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Dead);
    }

    #[test]
    fn absent_pod_with_active_activity_is_not_dead() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Absent);
        ev.activity = ActivitySignal::Active;

        let result = classify(&ev);
        // Active activity overrides absent pod — session may be between pod
        // transitions. Classifier returns Live since no higher-precedence
        // condition matches.
        assert_ne!(result.verdict, Verdict::Dead);
    }

    // ── Precedence 5: Slow + extension eligibility ───────────────────────

    #[test]
    fn running_pod_with_idle_activity_is_slow() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Running);
        ev.activity = ActivitySignal::Idle;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Slow);
        assert!(result.extension_eligible);
        assert_eq!(result.outcome, None);
        assert_eq!(result.reason, None);
    }

    #[test]
    fn slow_with_exhausted_budget_is_not_extension_eligible() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Running);
        ev.activity = ActivitySignal::Idle;
        ev.extension_budget_exhausted = true;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Slow);
        assert!(!result.extension_eligible);
        assert_eq!(result.outcome, Some(LivenessOutcome::SlowExtended));
        assert_eq!(
            result.reason,
            Some(LivenessReason::SlowExtensionBudgetExhausted)
        );
    }

    #[test]
    fn running_pod_with_never_active_is_slow() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Running);
        ev.activity = ActivitySignal::NeverActive;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Slow);
        assert!(result.extension_eligible);
    }

    // ── Live default ─────────────────────────────────────────────────────

    #[test]
    fn all_green_is_live() {
        let ev = live_evidence();
        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Live);
        assert_eq!(result.outcome, None);
        assert_eq!(result.reason, None);
        assert!(!result.extension_eligible);
    }

    #[test]
    fn pending_pod_with_active_signal_is_live() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Pending);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Live);
    }

    #[test]
    fn unknown_pod_with_active_signal_is_live() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Unknown);

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Live);
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn no_task_status_is_not_terminal() {
        let mut ev = live_evidence();
        ev.db_task_status = None;
        ev.pod_phase = Some(PodPhase::Running);
        ev.activity = ActivitySignal::Active;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Live);
        assert_ne!(result.outcome, Some(LivenessOutcome::KillNoop));
    }

    /// Absence is not guilt. A violation needs POSITIVE evidence on both axes,
    /// so a missing session row can no longer convict an exited pod — the
    /// asymmetry that made unknown evidence land in `ProtocolViolation`
    /// deterministically.
    #[test]
    fn no_session_status_with_exited_pod_is_not_a_protocol_violation() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Succeeded);
        ev.exit_code = Some(0);
        ev.db_session_status = None; // no session row

        let result = classify(&ev);
        assert_ne!(result.verdict, Verdict::ProtocolViolation);
        assert_ne!(result.outcome, Some(LivenessOutcome::ProtocolViolation));
    }

    /// The mirror image: a missing task row must not convict either. Before
    /// this, `db_task_status = None` failed precedence 1's `is_some_and` guard
    /// AND satisfied this branch, so it was a guaranteed violation.
    #[test]
    fn no_task_status_with_exited_pod_is_not_a_protocol_violation() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Succeeded);
        ev.exit_code = Some(0);
        ev.db_session_status = Some(DbSessionStatus::Running);
        ev.db_task_status = None; // no task row

        let result = classify(&ev);
        assert_ne!(result.verdict, Verdict::ProtocolViolation);
    }

    /// A session that exits with its task parked at a recorded handoff did its
    /// job. Every `is_settled` status must be exonerated, on both a clean and a
    /// crashing exit.
    #[test]
    fn exit_at_a_recorded_handoff_is_not_a_protocol_violation() {
        for status in [
            DbTaskStatus::NeedsTaskReview,
            DbTaskStatus::InTaskReview,
            DbTaskStatus::Approved,
            DbTaskStatus::PrDraft,
            DbTaskStatus::PrReview,
            DbTaskStatus::NeedsLeadIntervention,
            DbTaskStatus::InLeadIntervention,
        ] {
            for (phase, code) in [(PodPhase::Succeeded, 0), (PodPhase::Failed, 1)] {
                let mut ev = live_evidence();
                ev.pod_phase = Some(phase);
                ev.exit_code = Some(code);
                ev.db_session_status = Some(DbSessionStatus::Running);
                ev.db_task_status = Some(status);

                let result = classify(&ev);
                assert_ne!(
                    result.verdict,
                    Verdict::ProtocolViolation,
                    "{status:?} is a recorded handoff, not a structural inconsistency"
                );
            }
        }
    }

    /// The detector must keep firing on the shape it exists for: a pod that
    /// exited leaving its task still claimed or still queued, with the session
    /// row never settled.
    #[test]
    fn exit_leaving_the_task_unsettled_is_still_a_protocol_violation() {
        for status in [DbTaskStatus::Open, DbTaskStatus::InProgress] {
            let mut clean = live_evidence();
            clean.pod_phase = Some(PodPhase::Succeeded);
            clean.exit_code = Some(0);
            clean.db_session_status = Some(DbSessionStatus::Running);
            clean.db_task_status = Some(status);
            let result = classify(&clean);
            assert_eq!(result.verdict, Verdict::ProtocolViolation, "{status:?}");
            assert_eq!(result.reason, Some(LivenessReason::CleanExitNonterminal));

            let mut crashed = clean.clone();
            crashed.pod_phase = Some(PodPhase::Failed);
            crashed.exit_code = Some(1);
            let result = classify(&crashed);
            assert_eq!(result.verdict, Verdict::ProtocolViolation, "{status:?}");
            assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
        }
    }

    /// A non-settled status is ambiguous on its own. The task's own transition
    /// record disambiguates: a last transition OUT of a session-held status
    /// means a live session put the task there deliberately (the reviewer
    /// rejection path, `in_task_review → open`), so the exiting session
    /// completed its protocol and must NOT be convicted.
    #[test]
    fn handoff_off_a_session_held_status_exonerates_an_unsettled_task() {
        for status in [DbTaskStatus::Open, DbTaskStatus::InProgress] {
            let mut ev = live_evidence();
            ev.pod_phase = Some(PodPhase::Succeeded);
            ev.exit_code = Some(0);
            ev.db_session_status = Some(DbSessionStatus::Running);
            ev.db_task_status = Some(status);
            ev.handed_off_from_session_held_status = true;

            let result = classify(&ev);
            assert_ne!(
                result.verdict,
                Verdict::ProtocolViolation,
                "{status:?} reached by a deliberate handoff is not a violation"
            );
            assert_ne!(result.reason, Some(LivenessReason::CleanExitNonterminal));
        }
    }

    /// The statuses that only a live session holds. A transition OUT of one of
    /// these is a handoff; a transition INTO one is just a claim.
    #[test]
    fn session_held_statuses_are_the_active_ones() {
        for held in ["in_progress", "in_task_review", "in_lead_intervention"] {
            assert!(is_session_held_status(held), "{held} is session-held");
        }
        for not_held in [
            "open",
            "needs_task_review",
            "approved",
            "pr_draft",
            "pr_review",
            "needs_lead_intervention",
            "closed",
        ] {
            assert!(
                !is_session_held_status(not_held),
                "{not_held} is not session-held"
            );
        }
    }

    #[test]
    fn no_session_status_with_running_pod_is_live() {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Running);
        ev.db_session_status = None;
        ev.activity = ActivitySignal::Active;

        let result = classify(&ev);
        assert_eq!(result.verdict, Verdict::Live);
    }

    #[test]
    fn evidence_is_echoed_in_result() {
        let ev = live_evidence();
        let result = classify(&ev);
        // The evidence should be cloned into the result for audit
        assert_eq!(result.evidence.pod_phase, ev.pod_phase);
        assert_eq!(result.evidence.activity, ev.activity);
    }

    // ── Serialization round-trips ────────────────────────────────────────

    #[test]
    fn verdict_serde_round_trip() {
        for v in [
            Verdict::Live,
            Verdict::Slow,
            Verdict::Dead,
            Verdict::ProtocolViolation,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: Verdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn outcome_serde_round_trip() {
        let outcomes = [
            LivenessOutcome::Success,
            LivenessOutcome::Crash,
            LivenessOutcome::Timeout,
            LivenessOutcome::DeadReclaimed,
            LivenessOutcome::ProtocolViolation,
            LivenessOutcome::KillNoop,
            LivenessOutcome::SlowExtended,
        ];
        for o in outcomes {
            let json = serde_json::to_string(&o).unwrap();
            let back: LivenessOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(o, back);
        }
    }

    #[test]
    fn reason_as_str_matches_expected() {
        assert_eq!(
            LivenessReason::CleanExitNonterminal.as_str(),
            "clean_exit_nonterminal"
        );
        assert_eq!(
            LivenessReason::NonzeroExitNonterminal.as_str(),
            "nonzero_exit_nonterminal"
        );
        assert_eq!(
            LivenessReason::HardRuntimeExceeded.as_str(),
            "hard_runtime_exceeded"
        );
        assert_eq!(
            LivenessReason::SlowExtensionBudgetExhausted.as_str(),
            "slow_extension_budget_exhausted"
        );
        assert_eq!(LivenessReason::None.as_str(), "none");
    }

    #[test]
    fn classification_result_serde_round_trip() {
        let ev = live_evidence();
        let result = classify(&ev);
        let json = serde_json::to_string(&result).unwrap();
        let back: ClassificationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.verdict, back.verdict);
        assert_eq!(result.outcome, back.outcome);
        assert_eq!(result.reason, back.reason);
        assert_eq!(result.extension_eligible, back.extension_eligible);
    }

    // ── Display impls ────────────────────────────────────────────────────

    #[test]
    fn verdict_display() {
        assert_eq!(Verdict::Live.to_string(), "live");
        assert_eq!(Verdict::Slow.to_string(), "slow");
        assert_eq!(Verdict::Dead.to_string(), "dead");
        assert_eq!(Verdict::ProtocolViolation.to_string(), "protocol_violation");
    }

    #[test]
    fn outcome_display() {
        assert_eq!(LivenessOutcome::Success.to_string(), "success");
        assert_eq!(LivenessOutcome::Crash.to_string(), "crash");
        assert_eq!(LivenessOutcome::Timeout.to_string(), "timeout");
        assert_eq!(LivenessOutcome::DeadReclaimed.to_string(), "dead_reclaimed");
        assert_eq!(
            LivenessOutcome::ProtocolViolation.to_string(),
            "protocol_violation"
        );
        assert_eq!(LivenessOutcome::KillNoop.to_string(), "kill_noop");
        assert_eq!(LivenessOutcome::SlowExtended.to_string(), "slow_extended");
    }

    // ── DbStatus helpers ─────────────────────────────────────────────────

    #[test]
    fn db_session_status_terminal() {
        assert!(DbSessionStatus::Completed.is_terminal());
        assert!(DbSessionStatus::Interrupted.is_terminal());
        assert!(DbSessionStatus::Failed.is_terminal());
        assert!(!DbSessionStatus::Running.is_terminal());
        assert!(!DbSessionStatus::Paused.is_terminal());
    }

    #[test]
    fn db_task_status_terminal() {
        assert!(DbTaskStatus::Closed.is_terminal());
        assert!(!DbTaskStatus::Open.is_terminal());
        assert!(!DbTaskStatus::InProgress.is_terminal());
        assert!(!DbTaskStatus::Approved.is_terminal());
    }
}
