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

/// Normalized DB session status. Maps from known
/// `djinn_core::models::SessionStatus` wire values. Missing or unrecognized
/// persisted values remain absent from [`LivenessEvidence`], rather than being
/// fabricated as `Running`.
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

    fn as_wire_status(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::NeedsTaskReview => "needs_task_review",
            Self::InTaskReview => "in_task_review",
            Self::Approved => "approved",
            Self::PrDraft => "pr_draft",
            Self::PrReview => "pr_review",
            Self::NeedsLeadIntervention => "needs_lead_intervention",
            Self::InLeadIntervention => "in_lead_intervention",
            Self::Closed => "closed",
        }
    }

    /// Whether this status is generally settled for task lifecycle callers.
    ///
    /// Session-exit classification overlays a narrower ownership test using
    /// [`is_session_held_status`] and
    /// [`LivenessEvidence::handed_off_from_session_held_status`]. Consequently,
    /// `InTaskReview` and `InLeadIntervention` retain their broad settled
    /// semantics here but do not intrinsically exonerate their owning session.
    ///
    /// The landed 7luh ordering makes the exit evidence decisive: apply the
    /// Required transition, cross the settlement barrier, persist terminal
    /// session settlement/event, then perform the coordinator's single task and
    /// transition read-back/classification. There is no retired
    /// settlement-before-transition transit window in this path.
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
    /// Positive evidence that the run died on a **transient upstream provider
    /// fault** — a 5xx (`server_is_overloaded` / `server_error`), a 502/503/504,
    /// or a stream that died mid-flight — rather than on anything this session
    /// or task did.
    ///
    /// This is the signal the exit path used to throw away. A session's terminal
    /// status has three values (`completed` / `failed` / `interrupted`) and the
    /// exit path folds them onto two pod phases plus a sentinel exit code, so
    /// "the worker crashed" and "the worker's provider 500'd" arrive here as the
    /// identical `(Failed, 1)` pair. The classifier then convicted both of a
    /// protocol violation with a `Crash` outcome, terminalized the live attempt,
    /// and let the task be reclaimed and force-closed — which is how a
    /// three-second OpenAI outage killed refinement round 3 of task `nr41` on
    /// 2026-07-29.
    ///
    /// Distinct from [`Self::handed_off_from_session_held_status`], which
    /// exonerates by showing the task was moved somewhere DELIBERATELY. This one
    /// exonerates by naming an external CAUSE. A transient provider fault leaves
    /// the task exactly where the session found it — there is no handoff to
    /// find — so neither term subsumes the other.
    ///
    /// Carrying it as its own term rather than as another `exit_code` value is
    /// deliberate: an exit code describes the process, and the process really
    /// did exit nonzero. What changed is WHO is at fault, and that is a separate
    /// fact, so it gets a separate field. `false` is the fail-safe default —
    /// absent evidence preserves the pre-existing verdict exactly.
    #[serde(default)]
    pub transient_provider_fault: bool,
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
    /// The run ended because its **upstream provider** failed transiently (5xx
    /// / overloaded / mid-flight stream death), not because the session or the
    /// task misbehaved. Retryable: the identical work succeeds on the next
    /// healthy backend, so the attempt must be recorded as environmental rather
    /// than as a crash or a protocol violation.
    TransientProviderFault,
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
            Self::TransientProviderFault => "transient_provider_fault",
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
/// 3. **Protocol violation** → an observed clean/nonzero exit on a task that is
///    positively known to be unsettled (still `open`/`in_progress`), with a
///    known session status and no evidence that a session deliberately handed
///    the task off to that status
/// 4. **Dead** → absent/failed pod with no recent activity
/// 5. **Slow** → activity signal absent/idle, pod running, below hard cap
/// 6. **Live** → default when other conditions don't match
///
/// One extra rung sits between 2 and 3: a pod that exited on a **transient
/// upstream provider fault** ([`LivenessEvidence::transient_provider_fault`]) is
/// never a protocol violation, because nothing about the protocol was violated —
/// the provider was down. It classifies `Dead` / `DeadReclaimed` with
/// [`LivenessReason::TransientProviderFault`] so the run is reclaimed and
/// redispatched instead of convicted and force-closed.
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

    // ── 2b. Transient upstream provider fault ───────────────────────────
    // A pod that exited because its PROVIDER failed transiently violated no
    // protocol: the session did exactly what it was supposed to do until an
    // upstream 500 / overload / mid-flight stream death took the conversation
    // away from it. Convicting it here is not a cosmetic mislabel — the
    // `ProtocolViolation` verdict is what terminalizes the live attempt as a
    // `Crash`, which in turn lets the task be reclaimed and force-closed
    // (2026-07-29, task `nr41`, refinement round 3: `server_is_overloaded` →
    // `protocol_violation` → `Crash` → `dead` → force_closed, all inside one
    // second).
    //
    // This rung outranks the protocol-violation rung rather than being folded
    // into it because the two answer different questions. Precedence 3 asks "is
    // the recorded state structurally inconsistent?" and its terms are all
    // POSITIVE evidence of inconsistency. A transient provider fault is positive
    // evidence of a CAUSE, and a known cause is precisely what makes the
    // inconsistency explicable. So it must be consulted first, not appended as
    // another exoneration term.
    //
    // The verdict is `Dead`/`DeadReclaimed`: the pod really is gone and its
    // resources really must be reclaimed. What differs from a crash is the
    // reason, and the reason is what downstream reads to decide the attempt is
    // environmental (no dispatch penalty, no quality strike) and the work is
    // redispatchable.
    if matches!(
        evidence.pod_phase,
        Some(PodPhase::Succeeded) | Some(PodPhase::Failed)
    ) && evidence.transient_provider_fault
    {
        return ClassificationResult {
            verdict: Verdict::Dead,
            evidence: evidence.clone(),
            outcome: Some(LivenessOutcome::DeadReclaimed),
            reason: Some(LivenessReason::TransientProviderFault),
            extension_eligible: false,
        };
    }

    // ── 3. Protocol violation detection ─────────────────────────────────
    // A protocol violation is a STRUCTURAL INCONSISTENCY, so every term must be
    // POSITIVE evidence. Absence is not guilt.
    //
    // Every term is positive evidence. Unknown task or session state is
    // fail-safe: it produces no violation and no exit-path destructive action.
    //
    // Queue/post-session destinations are exonerated by `is_settled`, but claim
    // states remain owned by a live session even where that broad helper also
    // calls them settled. Thus `in_progress`, `in_task_review`, and
    // `in_lead_intervention` all require positive handoff evidence.
    let pod_exited = matches!(
        evidence.pod_phase,
        Some(PodPhase::Succeeded) | Some(PodPhase::Failed)
    );
    let known_session_status = evidence.db_session_status.is_some();
    let task_requires_handoff = evidence
        .db_task_status
        .as_ref()
        .is_some_and(|s| is_session_held_status(s.as_wire_status()) || !s.is_settled());

    // A non-settled status is not by itself evidence of abandonment. The task's
    // own transition record decides: if the last status change moved the task
    // OUT of a session-held status, a session handed it on deliberately
    // (`in_task_review → open` is the reviewer-rejection path, which the
    // supervisor drives on EVERY explicit reject verdict — see
    // `TransitionAction::TaskReviewReject`). Only a task left sitting where its
    // session found it — claimed and unmoved — is genuinely stranded.
    let handed_off = evidence.handed_off_from_session_held_status;

    if pod_exited && known_session_status && task_requires_handoff && !handed_off {
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
