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
    ///
    /// # `InTaskReview` and `InLeadIntervention` are settled here despite NOT being handoffs
    ///
    /// This is the one place where the list is knowingly wider than its own
    /// definition, and it must not be "tidied" without the evidence below.
    ///
    /// `needs_task_review` / `needs_lead_intervention` are QUEUE statuses — a
    /// worker really did hand the task to somebody else. `in_task_review` /
    /// `in_lead_intervention` are CLAIM statuses: only a live session holds
    /// them ([`is_session_held_status`] names exactly these plus
    /// `in_progress`). A reviewer's own handoff destinations are `approved`
    /// (approve) and `open` (reject) — never `in_task_review`. So a session
    /// exiting with its task still at `in_task_review` handed nothing on, and
    /// by this doc's own rule should be convicted.
    ///
    /// Concretely, that lets one real failure through. A reviewer that ends
    /// without calling `submit_review` maps to a non-terminal
    /// `StageOutcome::Failed` (`supervisor_impl/stage.rs`, the `""` arm of
    /// `reviewer_stage_outcome`) which deliberately performs NO transition, so
    /// the task stays `in_task_review`. That path is reached from the
    /// reply loop's `Ok(())` branch, so `final_result_ok` is true and
    /// `session_settlement_for_stage_outcome` settles the session
    /// **`completed`**, not `failed` — the pod exits 0. The exit classifier
    /// therefore sees `Succeeded` / exit 0 / session `running` / task
    /// `in_task_review`, and only this `is_settled` entry keeps it out of
    /// precedence 3. It falls through every later branch (`Succeeded` is not
    /// absent-or-failed, and not running) and lands on `Live`.
    ///
    /// # Why narrowing it is NOT safe today
    ///
    /// The exit classifier cannot tell that abandonment apart from a healthy
    /// reviewer, because of the write ordering in the supervisor stage:
    ///
    /// 1. the session row is settled first, which publishes
    ///    `session.completed` and drives `classify_session_exit_liveness`;
    /// 2. the reviewer's `task_review_approve` / `task_review_reject`
    ///    transition is issued only afterwards, on a separate RPC.
    ///
    /// At the instant the event fires, both cases are byte-identical on every
    /// axis this classifier reads: task at `in_task_review`, last transition
    /// `needs_task_review → in_task_review` (INTO a session-held status, so
    /// `handed_off_from_session_held_status` is false for both). Everything
    /// the reviewer decided — the transition, `task_runs`, `task_attempts`,
    /// the acceptance-criteria array — is written after the session row, by
    /// construction. Un-settling `InTaskReview` would therefore convict every
    /// reviewer whose transition is still in flight: the same false-positive
    /// class #2748 removed at `open`, reintroduced on a different status.
    ///
    /// The gates that DO close this race live in the stuck scan
    /// (`session_recovery.rs`: `pool.has_session` + the background-work
    /// tracker) and are not portable here — in the pod topology the
    /// post-session work registers on the agent's context, not the
    /// coordinator's tracker, and the transition is issued by the pod-side
    /// supervisor loop rather than by `spawn_post_session_work` at all.
    ///
    /// # What is lost, and what it would take to reclaim it
    ///
    /// Only signal. The verdict is observational — the caller in `actor.rs`
    /// discards the result, and the attempt-terminalizing step is gated on
    /// `failed`/`interrupted`, which a no-verdict reviewer exit is not. The
    /// operational hole is already closed elsewhere: the attempt is
    /// terminalized `Crashed` by the supervisor runner, `task_runs` records
    /// `failed`, and the stuck scan releases `in_task_review →
    /// needs_task_review` on its 30 s tick with no age threshold.
    ///
    /// To decide this properly, split the reviewer sessions whose persisted
    /// evidence shows `db_task_status = in_task_review` by how the task later
    /// left that status: `task_review_approve`/`task_review_reject` by
    /// `agent-supervisor` is a race and must stay exonerated;
    /// `release_task_review` by `coordinator` is a genuine abandonment. If the
    /// second group is material, the fix is to move the session settlement
    /// after the transition — only then can this list be narrowed.
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
#[path = "liveness_tests.rs"]
mod tests;
