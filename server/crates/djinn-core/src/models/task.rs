// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ── ReopenClass ──────────────────────────────────────────────────────────────

/// Typed classification of a reopen-like transition.
///
/// Persisted in status-transition activity payloads as `"reopen_class"` so the
/// repository layer can compute quality-strike counts without re-deriving
/// semantics from the action/status pair.  Historical activity rows that lack
/// the field are read as [`ReopenClass::Other`] (conservative default).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReopenClass {
    /// Reviewer rejected the implementation (TaskReviewReject,
    /// TaskReviewRejectStale, PrChangesRequested).
    ReviewRejected,
    /// Merge queue or CI failed (PrCiFailed).
    MergeQueueFailed,
    /// Merge conflict detected (TaskReviewRejectConflict, PrConflict,
    /// LeadApproveConflict).  These do NOT increment raw `reopen_count` and
    /// are excluded from quality-strike counts.
    MergeConflict,
    /// Task was superseded by newer work.
    Superseded,
    /// Infrastructure / provider-attempt failure that should NOT count as a
    /// worker/task-quality strike. Covers worker handshake timeouts, provider
    /// stalls, spawn failures, timed-out attempts, and crashed infra attempts
    /// (sourced from `task_attempts.outcome` values such as `timed_out`,
    /// `spawn_failed`, `crashed`). Excluded from quality-strike counts,
    /// intervention counters, and park escalation thresholds while still
    /// appearing in truthful park/retry diagnostics.
    Infra,
    /// Catch-all for reopen events whose specific class is unknown or
    /// missing from the activity payload (historical default).
    #[default]
    Other,
}

impl ReopenClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReviewRejected => "review_rejected",
            Self::MergeQueueFailed => "merge_queue_failed",
            Self::MergeConflict => "merge_conflict",
            Self::Superseded => "superseded",
            Self::Infra => "infra",
            Self::Other => "other",
        }
    }

    /// Parse from a wire string.  Unknown values resolve to
    /// [`ReopenClass::Other`] (conservative).
    pub fn parse(s: &str) -> Self {
        match s {
            "review_rejected" => Self::ReviewRejected,
            "merge_queue_failed" => Self::MergeQueueFailed,
            "merge_conflict" => Self::MergeConflict,
            "superseded" => Self::Superseded,
            "infra" => Self::Infra,
            _ => Self::Other,
        }
    }

    /// Returns `true` when this class counts as a quality strike
    /// (i.e. should be included in `quality_reopen_count`).
    pub fn is_quality_strike(&self) -> bool {
        matches!(
            self,
            Self::ReviewRejected | Self::MergeQueueFailed | Self::Other
        )
    }
}

impl std::fmt::Display for ReopenClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single entry in the typed reopen ledger returned by
/// [`TaskRepository::recent_reopen_ledger`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReopenLedgerEntry {
    /// The classified reopen cause.
    pub reopen_class: ReopenClass,
    /// ISO-8601 timestamp when this reopen was recorded.
    pub created_at: String,
    /// Status the task transitioned from.
    pub from_status: String,
    /// Free-form reason attached to the transition, if any.
    pub reason: Option<String>,
}

// ── CiStatus ──────────────────────────────────────────────────────────────────

/// Required-CI status for the current PR head.
///
/// Serializes/deserializes to/from the exact wire strings consumed by the
/// backend DTOs and (eventually) the frontend. These values are intentionally
/// independent of `TaskStatus`; lifecycle policy (e.g. `awaiting_ci`) is
/// derived downstream, not encoded here.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    /// All required checks passed on the current head.
    Passing,
    /// At least one required check failed on the current head.
    Failing,
    /// Required checks are still running; no blocking failure yet.
    Pending,
    /// CI state is not known (e.g. PR not yet polled, no checks present).
    #[default]
    Unknown,
}

impl CiStatus {
    /// The wire/JSON string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passing => "passing",
            Self::Failing => "failing",
            Self::Pending => "pending",
            Self::Unknown => "unknown",
        }
    }

    /// Parse from a wire/JSON string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "passing" => Ok(Self::Passing),
            "failing" => Ok(Self::Failing),
            "pending" => Ok(Self::Pending),
            "unknown" => Ok(Self::Unknown),
            other => Err(Error::Internal(format!("unknown ci_status: {other}"))),
        }
    }
}

impl std::fmt::Display for CiStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── TaskPrCiSnapshot ──────────────────────────────────────────────────────────

/// Durable snapshot of required-CI state for a task's current PR head.
///
/// This is the core data contract consumed by repository, DTO, and pr_poller
/// follow-up tasks. It intentionally carries no lifecycle policy: callers
/// combine `ci_status` with the task's `TaskStatus` to decide transitions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPrCiSnapshot {
    pub task_id: String,
    pub pr_number: i64,
    pub head_sha: String,
    pub ci_status: CiStatus,
    /// Names of required checks that are currently failing and blocking merge.
    pub blocking_required_check_names: Vec<String>,
    /// Stable fingerprint of the current failure signature (e.g. sorted failing
    /// check names + head SHA). Used by downstream remediation escalation to
    /// detect unchanged-head repeated failures.
    pub failure_fingerprint: Option<String>,
    /// ISO-8601 timestamp when this snapshot was first observed.
    pub first_seen_at: String,
    /// ISO-8601 timestamp when this snapshot was last observed/updated.
    pub last_seen_at: String,
    /// How many consecutive observations have carried the same
    /// `failure_fingerprint` for the same `head_sha`.
    pub same_signature_count: i64,
    /// Base SHA of the last remediation attempt for this failing signature.
    /// `None` when no remediation has been attempted yet.
    pub last_remediation_base_sha: Option<String>,
}

/// Input shape for creating or upserting a [`TaskPrCiSnapshot`].
///
/// Separate from the full snapshot so callers that do not yet know the
/// persisted `first_seen_at` (e.g. the pr_poller) can supply the observed
/// fields and let the repository layer resolve timestamps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPrCiSnapshotInput {
    pub task_id: String,
    pub pr_number: i64,
    pub head_sha: String,
    pub ci_status: CiStatus,
    pub blocking_required_check_names: Vec<String>,
    pub failure_fingerprint: Option<String>,
    pub same_signature_count: i64,
    pub last_remediation_base_sha: Option<String>,
}

impl TaskPrCiSnapshot {
    /// Build a snapshot from an input plus resolved timestamps.
    pub fn from_input(
        input: TaskPrCiSnapshotInput,
        first_seen_at: String,
        last_seen_at: String,
    ) -> Self {
        Self {
            task_id: input.task_id,
            pr_number: input.pr_number,
            head_sha: input.head_sha,
            ci_status: input.ci_status,
            blocking_required_check_names: input.blocking_required_check_names,
            failure_fingerprint: input.failure_fingerprint,
            first_seen_at,
            last_seen_at,
            same_signature_count: input.same_signature_count,
            last_remediation_base_sha: input.last_remediation_base_sha,
        }
    }
}

// ── IssueType ─────────────────────────────────────────────────────────────────

/// All recognised task issue types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueType {
    /// Standard implementation task — full worker lifecycle with review.
    Task,
    /// A product feature — same full lifecycle as `task`.
    Feature,
    /// A bug fix — same full lifecycle as `task`.
    Bug,
    /// Feasibility investigation — simple lifecycle (open → in_progress → closed).
    Spike,
    /// Open-ended research — simple lifecycle (open → in_progress → closed).
    Research,
    /// Epic/task planning — simple lifecycle, routed to Planner.
    /// Covers wave decomposition, epic metadata updates, memory-ref attachment, and re-prioritization.
    Planning,
    /// Architecture/code review — simple lifecycle, routed to Architect.
    Review,
    /// Proposal decomposition — simple lifecycle, routed to Planner.
    /// One per graduated proposal: the Planner reads the proposal spec and the
    /// target repos and creates the epics (with cross-repo dependencies). Has
    /// no `epic_id` (it operates one level above epics).
    EpicBreakdown,
    /// Proposal-refinement tribunal session — simple lifecycle
    /// (open → in_progress → closed). Routed to the refinement tribunal
    /// (advocate, adversary, or judge) via `SupervisorFlow::Refinement`.
    /// The `agent_type` field on the task determines the concrete tribunal role.
    Refinement,
}

impl IssueType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Feature => "feature",
            Self::Bug => "bug",
            Self::Spike => "spike",
            Self::Research => "research",
            Self::Planning => "planning",
            Self::Review => "review",
            Self::EpicBreakdown => "epic_breakdown",
            Self::Refinement => "refinement",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "task" => Ok(Self::Task),
            "feature" => Ok(Self::Feature),
            "bug" => Ok(Self::Bug),
            "spike" => Ok(Self::Spike),
            "research" => Ok(Self::Research),
            "planning" => Ok(Self::Planning),
            // Backward compat: existing DB rows may still say "decomposition".
            "decomposition" => Ok(Self::Planning),
            "review" => Ok(Self::Review),
            "epic_breakdown" => Ok(Self::EpicBreakdown),
            "refinement" => Ok(Self::Refinement),
            other => Err(Error::Internal(format!("unknown issue_type: {other}"))),
        }
    }

    /// Returns `true` for types that use the simple lifecycle
    /// (open → in_progress → closed), skipping review phases.
    pub fn uses_simple_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::Spike
                | Self::Research
                | Self::Planning
                | Self::Review
                | Self::EpicBreakdown
                | Self::Refinement
        )
    }
}

/// System-only priority level that sorts above P0.
/// MCP tools reject -1, so only the coordinator/system can set this.
pub const PRIORITY_CRITICAL: i64 = -1;

/// Title prefix marking a Planner `epic_breakdown` task as a proposal *review*
/// (dispatched when a graduated epic of a `building` proposal closes, to
/// reconcile acceptance criteria) rather than the initial proposal
/// decomposition. The coordinator stamps it onto the task title; the Planner
/// role selects the review prompt from it. Single source of truth so the
/// producer and consumer never drift.
pub const PROPOSAL_REVIEW_TITLE_PREFIX: &str = "Review proposal";

// ── close_reason literals ─────────────────────────────────────────────────────
//
// `close_reason` is a free-form `Option<String>` column (no DB enum) so these
// values are conventions, not a schema constraint.  Centralizing them as
// constants lets ADR-051 §7 reentrance guards filter by name without string
// literal drift.
//
// The first two are already emitted by the state machine; the last three are
// reserved for Planner-driven reshape force-closes (see ADR-051 §7).

/// Natural task completion (Close / PrMerge transitions).
pub const CLOSE_REASON_COMPLETED: &str = "completed";
/// Force-close (ForceClose / UserOverride → Closed transitions).
pub const CLOSE_REASON_FORCE_CLOSED: &str = "force_closed";
/// Planner force-closed this task as part of a board reshape.  Auto-dispatch
/// of a new planning wave is suppressed on this reason (ADR-051 §7).
pub const CLOSE_REASON_RESHAPE: &str = "reshape";
/// Planner force-closed this task because newer work supersedes it.
/// Auto-dispatch of a new planning wave is suppressed on this reason.
pub const CLOSE_REASON_SUPERSEDED: &str = "superseded";
/// Planner force-closed this task as a duplicate of another.
/// Auto-dispatch of a new planning wave is suppressed on this reason.
pub const CLOSE_REASON_DUPLICATE: &str = "duplicate";

/// Task board work item, always scoped under an epic.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub short_id: String,
    pub epic_id: Option<String>,
    pub title: String,
    pub description: String,
    pub design: String,
    pub issue_type: String,
    pub status: String,
    pub priority: i64,
    pub owner: String,
    /// JSON array of label strings.
    pub labels: String,
    /// JSON array of acceptance-criteria objects.
    pub acceptance_criteria: String,
    pub reopen_count: i64,
    pub continuation_count: i64,
    pub total_reopen_count: i64,
    pub intervention_count: i64,
    pub last_intervention_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
    /// URL of the GitHub PR created when the GitHub App is connected.
    /// NULL when the direct-push merge path is used (no GitHub App).
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub pr_url: Option<String>,
    /// JSON metadata about an active merge conflict (set by conflict transitions
    /// and worktree rebase failures; cleared on submit_task_review/close).
    pub merge_conflict_metadata: Option<String>,
    /// JSON array of memory note permalinks associated with this task.
    pub memory_refs: String,
    /// Specialist role name assigned to this task by the Planner (e.g. "rust-expert").
    /// When set, the slot lifecycle loads this Agent instead of the project default.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub agent_type: Option<String>,
    /// Stable `users.id` of whoever created this task. Stamped from the
    /// session user at the MCP dispatch root; for background/agent callers
    /// with no session it falls back to the parent epic's creator (so
    /// Planner-spawned tasks inherit the human who owns the epic). `None`
    /// only for tasks with neither a session user nor an owned epic.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub created_by_user_id: Option<String>,
    /// Promoted current-head PR CI status from `task_pr_ci_snapshots`.
    /// Defaults to `unknown` when no snapshot exists for the task PR.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_status: String,
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_head_sha: Option<String>,
    /// GitHub PR number for the promoted CI snapshot, when one exists.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_pr_number: Option<i64>,
    /// JSON array of blocking required check names for the current PR head.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_blocking_required_check_names: String,
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_failure_fingerprint: Option<String>,
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_first_seen_at: Option<String>,
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_last_seen_at: Option<String>,
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_same_signature_count: i64,
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_last_remediation_base_sha: Option<String>,
    // ── CI head reconciliation (m116) ──────────────────────────────────
    //
    // These fields are populated from the **latest task attempt** that
    // carries mirror/GitHub head evidence, NOT from `task_pr_ci_snapshots`.
    // They exist so operators and coordinators can see whether the internal
    // mirror branch head matches the GitHub PR branch head.  `ci_head_sha`
    // above (the CI snapshot head) is untouched — it remains the
    // GitHub/PR CI snapshot head from `task_pr_ci_snapshots.head_sha`.
    //
    // The mirror branch head SHA recorded by the most recent task attempt,
    // when known.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_mirror_head_sha: Option<String>,
    // The GitHub PR branch head SHA recorded by the most recent task attempt,
    // when known.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_github_head_sha: Option<String>,
    // `Some(true)` only when both heads are known and differ; `Some(false)`
    // only when both heads are known and equal; `None` when either side is
    // unknown.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_heads_diverged: Option<bool>,
    // Concise nullable error string from the most recent publication /
    // branch-head observation failure, when one is recorded.  Absent
    // (`None`) when no error is known.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub ci_head_observation_error: Option<String>,
    /// Number of unresolved blocker tasks (blocking tasks not yet closed).
    /// Populated by list queries via subquery; defaults to 0 elsewhere.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub unresolved_blocker_count: i64,
}

// ── Evidence-spike detection ──────────────────────────────────────────────────

/// Label stamped on evidence-spike tasks by the Judge demand-evidence tool
/// (`proposal_refinement_demand_evidence` in epic `6tjy`).
pub const EVIDENCE_SPIKE_LABEL: &str = "refinement-evidence";

/// Companion read-only marker stamped alongside [`EVIDENCE_SPIKE_LABEL`].
pub const EVIDENCE_SPIKE_READ_ONLY_LABEL: &str = "read-only";

/// Returns `true` when `labels` (a JSON-array string) carries both the
/// `refinement-evidence` and `read-only` markers that identify an
/// evidence-spike task created by the Judge demand-evidence path.
///
/// This is the canonical detection point for the evidence-spike runtime
/// profile.  The function is intentionally strict: both labels must be
/// present.  A task that carries only one marker (or has malformed JSON)
/// is **not** treated as an evidence spike — callers that need fail-closed
/// behavior should treat `false` as "deny mutation access".
pub fn is_evidence_spike(labels: &str) -> bool {
    labels.contains(EVIDENCE_SPIKE_LABEL) && labels.contains(EVIDENCE_SPIKE_READ_ONLY_LABEL)
}

/// A single entry in the task activity log (audit trail + comments).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ActivityEntry {
    pub id: String,
    pub task_id: Option<String>,
    pub actor_id: String,
    pub actor_role: String,
    pub event_type: String,
    /// JSON payload — shape varies by event_type.
    pub payload: String,
    pub created_at: String,
}

// ── State machine ─────────────────────────────────────────────────────────────

/// All valid task statuses. Serializes/deserializes to/from snake_case DB strings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    InProgress,
    NeedsTaskReview,
    InTaskReview,
    /// Reviewer approved; waiting for PR to be created (or GitHub App to create it).
    Approved,
    /// PR created as draft — CI running, not yet ready for human review.
    PrDraft,
    /// PR out of draft — awaiting human code review / merge.
    PrReview,
    NeedsLeadIntervention,
    InLeadIntervention,
    Closed,
}

impl TaskStatus {
    /// The DB/wire string representation.
    pub fn as_str(&self) -> &'static str {
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

    /// Parse from a DB/wire string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "in_progress" => Ok(Self::InProgress),
            "needs_task_review" => Ok(Self::NeedsTaskReview),
            "in_task_review" => Ok(Self::InTaskReview),
            "approved" => Ok(Self::Approved),
            "pr_draft" => Ok(Self::PrDraft),
            // backward compat: old pr_ready maps to pr_draft
            "pr_ready" => Ok(Self::PrDraft),
            "pr_review" => Ok(Self::PrReview),
            "needs_lead_intervention" => Ok(Self::NeedsLeadIntervention),
            "in_lead_intervention" => Ok(Self::InLeadIntervention),
            "closed" => Ok(Self::Closed),
            other => Err(Error::Internal(format!("unknown task status: {other}"))),
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Named transition actions matching the MCP `task_transition` tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionAction {
    Start,
    /// Worker re-entry from `needs_task_review` (the reviewer-response redo
    /// path). A task lands at `needs_task_review` only after the worker already
    /// submitted; when its branch is NOT durable the host routes a worker redo
    /// (`ReviewResponse`), but the worker can't walk `start` (legal only from
    /// `open`) so the run never moved off `needs_task_review` and the post-worker
    /// submission no-op'd — the task got review-dispatched without it. This action
    /// walks `needs_task_review → in_progress` so the redo ends with a legal
    /// `submit_task_review`. No AC/blocker gate
    /// (the task already passed `start` once).
    ResumeWorker,
    SubmitTaskReview,
    TaskReviewStart,
    TaskReviewReject,
    /// Reviewer rejects with no AC progress — increments continuation_count.
    TaskReviewRejectStale,
    TaskReviewRejectConflict,
    TaskReviewApprove,
    Close,
    Reopen,
    Release,
    ReleaseTaskReview,
    ForceClose,
    /// Administrative override: force a task to an arbitrary target status.
    /// **Must not** target `NeedsLeadIntervention` or `InLeadIntervention` —
    /// the arbiter lifecycle is only entered via `Escalate` (coordinator
    /// park-rung) and `LeadInterventionStart` (INVARIANT 10qg/aizl).
    UserOverride,
    /// System escalates stuck task to Lead intervention queue.
    Escalate,
    /// Lead agent starts working on an intervention task.
    LeadInterventionStart,
    /// Lead agent releases intervention (still needs attention).
    LeadInterventionRelease,
    /// Lead agent finishes intervention; task ready for worker again.
    LeadInterventionComplete,
    /// Lead agent approves implementation directly — triggers merge.
    LeadApprove,
    /// Merge conflict discovered during Lead approval — reopen for conflict resolution.
    LeadApproveConflict,
    /// Reviewer/Lead approves, moving task to approved (waiting for PR creation).
    PrCreated,
    /// GitHub App signals PR has been taken out of draft — transitions pr_draft → pr_review.
    PrUndraft,
    /// GitHub App signals CI failure on draft PR — transitions pr_draft → open.
    PrCiFailed,
    /// Merge conflict detected on approved or draft PR — transitions approved/pr_draft → open.
    PrConflict,
    /// GitHub App signals PR merged — transitions pr_review → closed.
    PrMerge,
    /// GitHub App signals changes requested on PR — transitions pr_review → open.
    PrChangesRequested,
    /// CI-loop remediation park: hold the source task by moving it back to
    /// `open` so its already-added remediation blocker keeps it out of dispatch
    /// (`list_ready` filters blocked-open tasks) until the remediation closes
    /// and `emit_unblocked_tasks` revives it. Unlike `PrCiFailed` / `Reopen`
    /// this is a HOLD, not a rework, so it does NOT increment `reopen_count`.
    ParkForRemediation,
    /// Non-worker role (planner/architect) completed with file changes —
    /// route through the approved → PR pipeline instead of closing directly.
    SubmitForMerge,
    /// Pre-approval CI-grade verification gate (proposal `uv3p`) rejected an
    /// approved submission whose focused Quality-Gate check set failed for the
    /// touched paths. Moves `approved → open` so the same task returns to a
    /// worker round carrying the gate feedback. Like `ParkForRemediation` this
    /// is a strike-free re-route, NOT a rework: it does NOT increment
    /// `reopen_count`, carries no `reopen_class`, and records no intervention,
    /// so a red CI-grade result never costs a reopen/quality strike.
    PreApprovalVerifyRejected,
    /// Arbiter `submit_decision(decision="park")` — the arbiter parked the
    /// task with a structured dossier. Moves `in_lead_intervention → open`
    /// behind a `HumanReview` remediation hold. The dossier is persisted on
    /// the arbitration row and the hold description. Like `ParkForRemediation`
    /// this is a HOLD, not a rework: it does NOT increment `reopen_count`.
    ArbiterPark,
    /// Arbiter `submit_decision(decision="supersede")` — the arbiter decomposed
    /// the task into replacement subtasks that carry the work forward, so the
    /// source task (and its PR) are force-closed as superseded. Moves
    /// `in_lead_intervention → closed` with force-closed semantics. The
    /// replacement subtasks and downstream blocker transfer are handled by the
    /// supervisor-side supersede transaction before this terminal move; no
    /// human-review hold is created. Terminal, like `ForceClose`.
    ArbiterSupersede,
}

impl TransitionAction {
    /// Whether this action requires a non-empty `reason` string.
    pub fn requires_reason(&self) -> bool {
        matches!(
            self,
            Self::TaskReviewReject
                | Self::TaskReviewRejectStale
                | Self::TaskReviewRejectConflict
                | Self::Reopen
                | Self::Release
                | Self::ReleaseTaskReview
                | Self::ForceClose
                | Self::Escalate
                | Self::LeadInterventionRelease
                | Self::PrChangesRequested
        )
    }

    /// Parse from a wire string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "start" => Ok(Self::Start),
            "resume_worker" => Ok(Self::ResumeWorker),
            "submit_task_review" => Ok(Self::SubmitTaskReview),
            "task_review_start" => Ok(Self::TaskReviewStart),
            "task_review_reject" => Ok(Self::TaskReviewReject),
            "task_review_reject_stale" => Ok(Self::TaskReviewRejectStale),
            "task_review_reject_conflict" => Ok(Self::TaskReviewRejectConflict),
            "task_review_approve" => Ok(Self::TaskReviewApprove),
            "close" => Ok(Self::Close),
            "reopen" => Ok(Self::Reopen),
            "release" => Ok(Self::Release),
            "release_task_review" => Ok(Self::ReleaseTaskReview),
            "force_close" => Ok(Self::ForceClose),
            "user_override" => Ok(Self::UserOverride),
            "escalate" => Ok(Self::Escalate),
            "lead_intervention_start" => Ok(Self::LeadInterventionStart),
            "lead_intervention_release" => Ok(Self::LeadInterventionRelease),
            "lead_intervention_complete" => Ok(Self::LeadInterventionComplete),
            "lead_approve" => Ok(Self::LeadApprove),
            "lead_approve_conflict" => Ok(Self::LeadApproveConflict),
            "pr_created" => Ok(Self::PrCreated),
            "pr_undraft" => Ok(Self::PrUndraft),
            "pr_ci_failed" => Ok(Self::PrCiFailed),
            "pr_conflict" => Ok(Self::PrConflict),
            "pr_merge" => Ok(Self::PrMerge),
            "pr_changes_requested" => Ok(Self::PrChangesRequested),
            "park_for_remediation" => Ok(Self::ParkForRemediation),
            "submit_for_merge" => Ok(Self::SubmitForMerge),
            "preapproval_verify_rejected" => Ok(Self::PreApprovalVerifyRejected),
            "arbiter_park" => Ok(Self::ArbiterPark),
            "arbiter_supersede" => Ok(Self::ArbiterSupersede),
            other => Err(Error::Internal(format!(
                "unknown transition action: {other}"
            ))),
        }
    }
}

/// The computed effect of a validated transition.
///
/// Returned by [`compute_transition`]; applied atomically by `TaskRepository::transition`.
pub struct TransitionApply {
    /// Target status.
    pub to_status: Option<TaskStatus>,
    /// Increment `reopen_count` by 1.
    pub increment_reopen: bool,
    /// Reset `continuation_count` to 0.
    pub reset_continuation: bool,
    /// Increment `continuation_count` by 1 (for stale reopen detection).
    pub increment_continuation: bool,
    /// Set `closed_at` to the current timestamp.
    pub set_closed_at: bool,
    /// Set `closed_at` to NULL.
    pub clear_closed_at: bool,
    /// Set `close_reason` to this value.
    pub close_reason: Option<&'static str>,
    /// Set `close_reason` to NULL.
    pub clear_close_reason: bool,
    /// Set merge_conflict_metadata to a value (caller provides the JSON).
    pub set_merge_conflict_metadata: bool,
    /// Clear merge_conflict_metadata to NULL.
    pub clear_merge_conflict_metadata: bool,
    /// Increment intervention_count and set last_intervention_at.
    pub record_intervention: bool,
    /// Value for `event_type` in the activity log entry.
    pub activity_type: &'static str,
    /// Typed classification of the reopen cause, persisted in the activity
    /// payload as `"reopen_class"`.  `None` for non-reopen-like transitions.
    pub reopen_class: Option<ReopenClass>,
}

impl Default for TransitionApply {
    fn default() -> Self {
        Self {
            to_status: None,
            increment_reopen: false,
            reset_continuation: false,
            increment_continuation: false,
            set_closed_at: false,
            clear_closed_at: false,
            close_reason: None,
            clear_close_reason: false,
            set_merge_conflict_metadata: false,
            clear_merge_conflict_metadata: false,
            record_intervention: false,
            activity_type: "status_changed",
            reopen_class: None,
        }
    }
}

impl TransitionApply {
    fn simple(to: TaskStatus) -> Self {
        Self {
            to_status: Some(to),
            ..Default::default()
        }
    }
}

/// Validate a transition and return the set of effects to apply.
///
/// Does **not** check unresolved blockers — the caller handles that for `Start`.
/// Does **not** touch the database.
pub fn compute_transition(
    action: &TransitionAction,
    from: &TaskStatus,
    target_override: Option<&TaskStatus>,
) -> Result<TransitionApply> {
    let bad = |msg: &str| Err(Error::InvalidTransition(msg.to_owned()));

    Ok(match action {
        TransitionAction::Start => {
            if *from != TaskStatus::Open {
                return bad("start is only valid from open");
            }
            TransitionApply::simple(TaskStatus::InProgress)
        }

        TransitionAction::ResumeWorker => {
            // Worker redo re-entry from a non-durable `needs_task_review`. Only
            // legal from `needs_task_review`; carries no AC/blocker gate (unlike
            // `Start`) because the task already cleared those when it first
            // started. Lands at `in_progress` so the post-worker
            // `submit_task_review` walk succeeds and the task enters review.
            if *from != TaskStatus::NeedsTaskReview {
                return bad("resume_worker is only valid from needs_task_review");
            }
            TransitionApply::simple(TaskStatus::InProgress)
        }

        TransitionAction::SubmitTaskReview => {
            if *from != TaskStatus::InProgress {
                return bad("submit_task_review is only valid from in_progress");
            }
            TransitionApply {
                to_status: Some(TaskStatus::NeedsTaskReview),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewStart => {
            if *from != TaskStatus::NeedsTaskReview {
                return bad("task_review_start is only valid from needs_task_review");
            }
            TransitionApply::simple(TaskStatus::InTaskReview)
        }

        TransitionAction::TaskReviewReject => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_reject is only valid from in_task_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                reopen_class: Some(ReopenClass::ReviewRejected),
                // continuation_count handled by circuit breaker (reset if progress, increment if stale)
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewRejectStale => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_reject_stale is only valid from in_task_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                increment_continuation: true,
                reopen_class: Some(ReopenClass::ReviewRejected),
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewRejectConflict => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_reject_conflict is only valid from in_task_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                set_merge_conflict_metadata: true,
                reopen_class: Some(ReopenClass::MergeConflict),
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewApprove => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_approve is only valid from in_task_review");
            }
            TransitionApply::simple(TaskStatus::Approved)
        }

        TransitionAction::Close => {
            if *from == TaskStatus::Closed {
                return bad("task is already closed");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some(CLOSE_REASON_COMPLETED),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::Reopen => {
            if *from != TaskStatus::Closed {
                return bad("reopen is only valid from closed");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                reset_continuation: true,
                clear_closed_at: true,
                clear_close_reason: true,
                reopen_class: Some(ReopenClass::Other),
                ..Default::default()
            }
        }

        TransitionAction::Release => {
            if *from != TaskStatus::InProgress {
                return bad("release is only valid from in_progress");
            }
            TransitionApply::simple(TaskStatus::Open)
        }

        TransitionAction::ReleaseTaskReview => {
            if *from != TaskStatus::InTaskReview {
                return bad("release_task_review is only valid from in_task_review");
            }
            TransitionApply::simple(TaskStatus::NeedsTaskReview)
        }

        TransitionAction::ForceClose => {
            if *from == TaskStatus::Closed {
                return bad("task is already closed");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some(CLOSE_REASON_FORCE_CLOSED),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::UserOverride => {
            let target = target_override.ok_or_else(|| {
                Error::InvalidTransition("user_override requires target_status".to_owned())
            })?;
            // INVARIANT (10qg/aizl): UserOverride must NOT bypass the
            // coordinator arbiter park-rung to enter the Lead intervention
            // lifecycle.  `NeedsLeadIntervention` is only reachable via
            // `Escalate` (coordinator park-rung) or
            // `LeadInterventionRelease` (coordinator session-recovery).
            // `InLeadIntervention` is only reachable via
            // `LeadInterventionStart` from `NeedsLeadIntervention`.
            // Guarded by `only_escalate_and_release_produce_needs_lead_intervention`.
            if matches!(
                target,
                TaskStatus::NeedsLeadIntervention | TaskStatus::InLeadIntervention
            ) {
                return bad("user_override must not target needs_lead_intervention or \
                     in_lead_intervention; use escalate/lead_intervention_start \
                     for the coordinator arbiter lifecycle");
            }
            let closing = *target == TaskStatus::Closed;
            TransitionApply {
                to_status: Some(target.clone()),
                reset_continuation: true,
                set_closed_at: closing,
                clear_closed_at: !closing,
                close_reason: if closing {
                    Some(CLOSE_REASON_FORCE_CLOSED)
                } else {
                    None
                },
                clear_close_reason: !closing,
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::Escalate => {
            // Widen to every non-terminal status the second-strike park rung
            // may observe, so the coordinator can route into
            // `NeedsLeadIntervention` (arbiter entry) from the same source
            // set that `ParkForRemediation` accepts.  Terminal statuses
            // (Closed) and the Lead intervention pair are intentionally
            // excluded: the arbiter entry point must not re-enter an
            // already-active Lead intervention or bypass close.
            //
            // INVARIANT (10qg/aizl): This is the ONLY production path that
            // transitions a task into `NeedsLeadIntervention`.  The
            // `only_escalate_and_release_produce_needs_lead_intervention`
            // test guards this invariant.  Worker/reviewer `request_lead`
            // calls are deprecated to Planner routing and must NOT reach
            // this transition.
            if !matches!(
                from,
                TaskStatus::Open
                    | TaskStatus::InProgress
                    | TaskStatus::NeedsTaskReview
                    | TaskStatus::InTaskReview
                    | TaskStatus::Approved
                    | TaskStatus::PrDraft
                    | TaskStatus::PrReview,
            ) {
                return bad(
                    "escalate is only valid from open, in_progress, needs_task_review, \
                     in_task_review, approved, pr_draft, or pr_review",
                );
            }
            TransitionApply {
                to_status: Some(TaskStatus::NeedsLeadIntervention),
                reset_continuation: true,
                ..Default::default()
            }
        }

        TransitionAction::LeadInterventionStart => {
            if *from != TaskStatus::NeedsLeadIntervention {
                return bad("lead_intervention_start is only valid from needs_lead_intervention");
            }
            TransitionApply::simple(TaskStatus::InLeadIntervention)
        }

        TransitionAction::LeadInterventionRelease => {
            // Coordinator session-recovery: releases an active Lead
            // intervention back to queued status.  This is the only other
            // production path (besides `Escalate`) that produces
            // `NeedsLeadIntervention`.  Guarded by
            // `only_escalate_and_release_produce_needs_lead_intervention`.
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_intervention_release is only valid from in_lead_intervention");
            }
            TransitionApply::simple(TaskStatus::NeedsLeadIntervention)
        }

        TransitionAction::LeadInterventionComplete => {
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_intervention_complete is only valid from in_lead_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                record_intervention: true,
                ..Default::default()
            }
        }

        TransitionAction::LeadApprove => {
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_approve is only valid from in_lead_intervention");
            }
            TransitionApply::simple(TaskStatus::Approved)
        }

        TransitionAction::LeadApproveConflict => {
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_approve_conflict is only valid from in_lead_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                set_merge_conflict_metadata: true,
                reopen_class: Some(ReopenClass::MergeConflict),
                ..Default::default()
            }
        }

        TransitionAction::PrCreated => {
            if *from != TaskStatus::Approved {
                return bad("pr_created is only valid from approved");
            }
            TransitionApply::simple(TaskStatus::PrDraft)
        }

        TransitionAction::PrUndraft => {
            if *from != TaskStatus::PrDraft {
                return bad("pr_undraft is only valid from pr_draft");
            }
            TransitionApply::simple(TaskStatus::PrReview)
        }

        TransitionAction::PrCiFailed => {
            // Valid both while the PR is still a draft (pre-undraft CI) and
            // after it's been undrafted into review — the merge queue can
            // reject an undrafted PR with `failed_checks`, and the poller's
            // `handle_queue_failure` reopens it via this action.
            if !matches!(from, TaskStatus::PrDraft | TaskStatus::PrReview) {
                return bad("pr_ci_failed is only valid from pr_draft or pr_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                reopen_class: Some(ReopenClass::MergeQueueFailed),
                ..Default::default()
            }
        }

        TransitionAction::PrConflict => {
            if !matches!(
                from,
                TaskStatus::Approved | TaskStatus::PrDraft | TaskStatus::PrReview
            ) {
                return bad("pr_conflict is only valid from approved, pr_draft, or pr_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                set_merge_conflict_metadata: true,
                reopen_class: Some(ReopenClass::MergeConflict),
                ..Default::default()
            }
        }

        TransitionAction::PrMerge => {
            // A merged PR is ground truth: the work landed, so the task must
            // close regardless of which pre-merge state it was observed in.
            // The merge can land while the task is still in `pr_draft` (merge
            // queue / auto-merge merges before the poller undrafts → moves it
            // to `pr_review`); rejecting it there wedged the PR poller in a
            // detect-merge → illegal-transition loop with the task stuck open.
            if !matches!(*from, TaskStatus::PrDraft | TaskStatus::PrReview) {
                return bad("pr_merge is only valid from pr_draft or pr_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some(CLOSE_REASON_COMPLETED),
                ..Default::default()
            }
        }

        TransitionAction::PrChangesRequested => {
            if *from != TaskStatus::PrReview {
                return bad("pr_changes_requested is only valid from pr_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                reopen_class: Some(ReopenClass::ReviewRejected),
                ..Default::default()
            }
        }

        TransitionAction::SubmitForMerge => {
            if *from != TaskStatus::InProgress {
                return bad("submit_for_merge is only valid from in_progress");
            }
            TransitionApply::simple(TaskStatus::Approved)
        }

        TransitionAction::ParkForRemediation => {
            // Park (hold) the source on its remediation blocker by landing it at
            // `open`. Legal from every pre-terminal in-flight state a CI-loop
            // park can observe, plus the Lead/arbiter hold states when bounded
            // arbiter accounting auto-parks; a no-op when the task is already
            // `open`. Unlike `PrCiFailed` / `Reopen` it does NOT bump
            // `reopen_count` — this is a hold pending remediation, not another
            // rework round.
            if !matches!(
                from,
                TaskStatus::PrDraft
                    | TaskStatus::PrReview
                    | TaskStatus::InProgress
                    | TaskStatus::Open
                    | TaskStatus::NeedsTaskReview
                    | TaskStatus::InTaskReview
                    | TaskStatus::NeedsLeadIntervention
                    | TaskStatus::InLeadIntervention
                    | TaskStatus::Approved
            ) {
                return bad(
                    "park_for_remediation is only valid from pr_draft, pr_review, in_progress, open, needs_task_review, in_task_review, needs_lead_intervention, in_lead_intervention, or approved",
                );
            }
            TransitionApply::simple(TaskStatus::Open)
        }

        TransitionAction::PreApprovalVerifyRejected => {
            // Pre-approval CI-grade verification gate (proposal `uv3p`) blocked
            // the approved → PR-push path because the focused Quality-Gate check
            // set failed for the touched paths. Return the task to a worker
            // round by landing it at `open`. This is deliberately strike-free:
            // no `increment_reopen`, no `reopen_class`, no `record_intervention`
            // — a red CI-grade result is delivered as feedback, not counted as a
            // reopen/quality strike. Only legal from `approved` (the seam the
            // coordinator enforces just before pushing the task branch).
            if *from != TaskStatus::Approved {
                return bad("preapproval_verify_rejected is only valid from approved");
            }
            TransitionApply::simple(TaskStatus::Open)
        }

        TransitionAction::ArbiterPark => {
            // Arbiter `submit_decision(decision="park")` — the arbiter parked
            // the task with a structured dossier. Like ParkForRemediation this
            // lands the source at `open` behind a HumanReview hold blocker;
            // unlike ParkForRemediation it is only legal from
            // `InLeadIntervention`. Does NOT bump `reopen_count` — this is a
            // hold, not a rework.
            if *from != TaskStatus::InLeadIntervention {
                return bad("arbiter_park is only valid from in_lead_intervention");
            }
            TransitionApply::simple(TaskStatus::Open)
        }

        TransitionAction::ArbiterSupersede => {
            // Arbiter `submit_decision(decision="supersede")` — the source was
            // decomposed into replacement subtasks that carry the work forward,
            // so it is force-closed as superseded. Only legal from
            // `InLeadIntervention`. Terminal (→ closed) with force-closed
            // semantics, identical to `ForceClose` but scoped to the arbiter
            // rung so the supervisor supersede transaction (arbitration-row
            // consume, blocker transfer, branch/PR cleanup) runs first.
            if *from != TaskStatus::InLeadIntervention {
                return bad("arbiter_supersede is only valid from in_lead_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some(CLOSE_REASON_FORCE_CLOSED),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }
    })
}

/// Validate a transition, routing to the appropriate lifecycle based on `issue_type`.
///
/// For `spike`, `research`, `decomposition`, and `review` task types the simple
/// lifecycle applies: `open → in_progress → closed`.  Actions that belong only to
/// the full worker lifecycle (task_review_*, lead_intervention_*) are rejected
/// for these types.
///
/// All other issue types (task, feature, bug) use the full lifecycle via
/// [`compute_transition`].
pub fn compute_transition_for_issue_type(
    action: &TransitionAction,
    from: &TaskStatus,
    target_override: Option<&TaskStatus>,
    issue_type: &str,
) -> Result<TransitionApply> {
    let uses_simple = IssueType::parse(issue_type)
        .map(|it| it.uses_simple_lifecycle())
        .unwrap_or(false);

    if uses_simple {
        // Restrict to actions that make sense in the simple lifecycle.
        let allowed = matches!(
            action,
            TransitionAction::Start
                | TransitionAction::Close
                | TransitionAction::ForceClose
                | TransitionAction::Reopen
                | TransitionAction::Release
                | TransitionAction::UserOverride
                | TransitionAction::Escalate
                | TransitionAction::LeadInterventionStart
                | TransitionAction::LeadInterventionRelease
                | TransitionAction::LeadInterventionComplete
                | TransitionAction::LeadApprove
                | TransitionAction::LeadApproveConflict
                | TransitionAction::SubmitForMerge
                | TransitionAction::PrCreated
                | TransitionAction::PrUndraft
                | TransitionAction::PrCiFailed
                | TransitionAction::PrConflict
                | TransitionAction::PrMerge
                | TransitionAction::PrChangesRequested
                | TransitionAction::ParkForRemediation
                | TransitionAction::ArbiterPark
                | TransitionAction::ArbiterSupersede
        );
        if !allowed {
            return Err(Error::InvalidTransition(format!(
                "action {action:?} is not valid for issue_type '{issue_type}' (simple lifecycle: open → in_progress → closed)"
            )));
        }
    }

    compute_transition(action, from, target_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUSES: [TaskStatus; 10] = [
        TaskStatus::Open,
        TaskStatus::InProgress,
        TaskStatus::NeedsTaskReview,
        TaskStatus::InTaskReview,
        TaskStatus::Approved,
        TaskStatus::PrDraft,
        TaskStatus::PrReview,
        TaskStatus::NeedsLeadIntervention,
        TaskStatus::InLeadIntervention,
        TaskStatus::Closed,
    ];

    const ACTIONS: [TransitionAction; 30] = [
        TransitionAction::Start,
        TransitionAction::ResumeWorker,
        TransitionAction::SubmitTaskReview,
        TransitionAction::TaskReviewStart,
        TransitionAction::TaskReviewReject,
        TransitionAction::TaskReviewRejectStale,
        TransitionAction::TaskReviewRejectConflict,
        TransitionAction::TaskReviewApprove,
        TransitionAction::Close,
        TransitionAction::Reopen,
        TransitionAction::Release,
        TransitionAction::ReleaseTaskReview,
        TransitionAction::ForceClose,
        TransitionAction::UserOverride,
        TransitionAction::Escalate,
        TransitionAction::LeadInterventionStart,
        TransitionAction::LeadInterventionRelease,
        TransitionAction::LeadInterventionComplete,
        TransitionAction::LeadApprove,
        TransitionAction::LeadApproveConflict,
        TransitionAction::PrCreated,
        TransitionAction::PrUndraft,
        TransitionAction::PrCiFailed,
        TransitionAction::PrConflict,
        TransitionAction::PrMerge,
        TransitionAction::PrChangesRequested,
        TransitionAction::ParkForRemediation,
        TransitionAction::PreApprovalVerifyRejected,
        TransitionAction::ArbiterPark,
        TransitionAction::ArbiterSupersede,
    ];

    fn expected_status(action: &TransitionAction, from: &TaskStatus) -> Option<TaskStatus> {
        match (action, from) {
            (TransitionAction::Start, TaskStatus::Open) => Some(TaskStatus::InProgress),
            (TransitionAction::ResumeWorker, TaskStatus::NeedsTaskReview) => {
                Some(TaskStatus::InProgress)
            }
            (TransitionAction::SubmitTaskReview, TaskStatus::InProgress) => {
                Some(TaskStatus::NeedsTaskReview)
            }
            (TransitionAction::TaskReviewStart, TaskStatus::NeedsTaskReview) => {
                Some(TaskStatus::InTaskReview)
            }
            (TransitionAction::TaskReviewReject, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::TaskReviewRejectStale, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::TaskReviewRejectConflict, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::TaskReviewApprove, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Approved)
            }
            (TransitionAction::Close, s) if *s != TaskStatus::Closed => Some(TaskStatus::Closed),
            (TransitionAction::Reopen, TaskStatus::Closed) => Some(TaskStatus::Open),
            (TransitionAction::Release, TaskStatus::InProgress) => Some(TaskStatus::Open),
            (TransitionAction::ReleaseTaskReview, TaskStatus::InTaskReview) => {
                Some(TaskStatus::NeedsTaskReview)
            }
            (TransitionAction::ForceClose, s) if *s != TaskStatus::Closed => {
                Some(TaskStatus::Closed)
            }
            (
                TransitionAction::Escalate,
                TaskStatus::Open
                | TaskStatus::InProgress
                | TaskStatus::NeedsTaskReview
                | TaskStatus::InTaskReview
                | TaskStatus::Approved
                | TaskStatus::PrDraft
                | TaskStatus::PrReview,
            ) => Some(TaskStatus::NeedsLeadIntervention),
            (TransitionAction::LeadInterventionStart, TaskStatus::NeedsLeadIntervention) => {
                Some(TaskStatus::InLeadIntervention)
            }
            (TransitionAction::LeadInterventionRelease, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::NeedsLeadIntervention)
            }
            (TransitionAction::LeadInterventionComplete, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::LeadApprove, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Approved)
            }
            (TransitionAction::LeadApproveConflict, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::PrCreated, TaskStatus::Approved) => Some(TaskStatus::PrDraft),
            (TransitionAction::PrUndraft, TaskStatus::PrDraft) => Some(TaskStatus::PrReview),
            (TransitionAction::PrCiFailed, TaskStatus::PrDraft | TaskStatus::PrReview) => {
                Some(TaskStatus::Open)
            }
            (
                TransitionAction::PrConflict,
                TaskStatus::Approved | TaskStatus::PrDraft | TaskStatus::PrReview,
            ) => Some(TaskStatus::Open),
            (TransitionAction::PrMerge, TaskStatus::PrDraft | TaskStatus::PrReview) => {
                Some(TaskStatus::Closed)
            }
            (TransitionAction::PrChangesRequested, TaskStatus::PrReview) => Some(TaskStatus::Open),
            (
                TransitionAction::ParkForRemediation,
                TaskStatus::PrDraft
                | TaskStatus::PrReview
                | TaskStatus::InProgress
                | TaskStatus::Open
                | TaskStatus::NeedsTaskReview
                | TaskStatus::InTaskReview
                | TaskStatus::Approved
                | TaskStatus::NeedsLeadIntervention
                | TaskStatus::InLeadIntervention,
            ) => Some(TaskStatus::Open),
            (TransitionAction::PreApprovalVerifyRejected, TaskStatus::Approved) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::ArbiterPark, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::ArbiterSupersede, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Closed)
            }
            (TransitionAction::SubmitForMerge, TaskStatus::InProgress) => {
                Some(TaskStatus::Approved)
            }
            _ => None,
        }
    }

    #[test]
    fn transition_matrix_valid_and_invalid_pairs() {
        for action in ACTIONS {
            for from in &STATUSES {
                if matches!(action, TransitionAction::UserOverride) {
                    continue;
                }
                let res = compute_transition(&action, from, None);
                match expected_status(&action, from) {
                    Some(to) => {
                        let apply = res.unwrap_or_else(|_| {
                            panic!("expected valid {:?} from {:?}", action, from)
                        });
                        assert_eq!(
                            apply.to_status,
                            Some(to),
                            "wrong to_status for {:?} from {:?}",
                            action,
                            from
                        );
                    }
                    None => {
                        assert!(
                            res.is_err(),
                            "expected invalid {:?} from {:?}",
                            action,
                            from
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn review_response_worker_redo_walk_reaches_task_review() {
        // Regression for the redo hop (task u4fx): a non-durable
        // `needs_task_review` routes a worker redo. The worker pre-stage can't
        // walk `start` (legal only from `open`), so the supervisor uses
        // `resume_worker` to move `needs_task_review → in_progress`; the
        // post-worker `submit_task_review` then succeeds and the task enters
        // `needs_task_review` — ready for the reviewer dispatch.
        let resume = compute_transition(
            &TransitionAction::ResumeWorker,
            &TaskStatus::NeedsTaskReview,
            None,
        )
        .expect("resume_worker from needs_task_review is valid");
        assert_eq!(resume.to_status, Some(TaskStatus::InProgress));

        let submit = compute_transition(
            &TransitionAction::SubmitTaskReview,
            &resume.to_status.clone().expect("resume sets status"),
            None,
        )
        .expect("submit_task_review from in_progress (after resume) is valid");
        assert_eq!(submit.to_status, Some(TaskStatus::NeedsTaskReview));
    }

    #[test]
    fn resume_worker_invalid_outside_needs_task_review() {
        // `resume_worker` is the redo-only re-entry; it must NOT be a backdoor
        // into in_progress from any other state (e.g. open uses `start` with its
        // AC/blocker gate; in_task_review has its own legal exits).
        for from in [
            TaskStatus::Open,
            TaskStatus::InProgress,
            TaskStatus::InTaskReview,
            TaskStatus::Approved,
            TaskStatus::Closed,
        ] {
            assert!(
                compute_transition(&TransitionAction::ResumeWorker, &from, None).is_err(),
                "resume_worker must be invalid from {from:?}"
            );
        }
    }

    #[test]
    fn user_override_requires_target_and_applies_target() {
        assert!(
            compute_transition(&TransitionAction::UserOverride, &TaskStatus::Open, None).is_err()
        );

        let closed = compute_transition(
            &TransitionAction::UserOverride,
            &TaskStatus::InProgress,
            Some(&TaskStatus::Closed),
        )
        .expect("closed override should be valid");
        assert_eq!(closed.to_status, Some(TaskStatus::Closed));
        assert!(closed.set_closed_at);
        assert_eq!(closed.close_reason, Some("force_closed"));
        assert!(!closed.clear_close_reason);

        let open = compute_transition(
            &TransitionAction::UserOverride,
            &TaskStatus::Closed,
            Some(&TaskStatus::Open),
        )
        .expect("open override should be valid");
        assert_eq!(open.to_status, Some(TaskStatus::Open));
        assert!(open.clear_closed_at);
        assert!(open.clear_close_reason);

        // INVARIANT (10qg/aizl): UserOverride must not bypass the
        // coordinator arbiter park-rung to enter the Lead intervention
        // lifecycle.
        assert!(
            compute_transition(
                &TransitionAction::UserOverride,
                &TaskStatus::Open,
                Some(&TaskStatus::NeedsLeadIntervention),
            )
            .is_err(),
            "user_override must not target needs_lead_intervention"
        );
        assert!(
            compute_transition(
                &TransitionAction::UserOverride,
                &TaskStatus::NeedsLeadIntervention,
                Some(&TaskStatus::InLeadIntervention),
            )
            .is_err(),
            "user_override must not target in_lead_intervention"
        );
    }

    #[test]
    fn continuation_escalation_threshold_behavior() {
        let stale = compute_transition(
            &TransitionAction::TaskReviewRejectStale,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("stale reject should be valid");
        assert!(stale.increment_continuation);
        assert!(stale.increment_reopen);

        let escalate = compute_transition(&TransitionAction::Escalate, &TaskStatus::Open, None)
            .expect("escalate should be valid from open");
        assert_eq!(escalate.to_status, Some(TaskStatus::NeedsLeadIntervention));
        assert!(escalate.reset_continuation);
    }

    /// A spike walks open → in_progress → closed: the supervisor moves it to
    /// in_progress when the Architect stage starts (so the board reflects the
    /// running architect instead of sitting `open`), then closes it on
    /// completion. Both transitions must be valid for the `spike` issue_type
    /// (simple lifecycle), with no acceptance-criteria gate on Start.
    #[test]
    fn spike_simple_lifecycle_walks_open_in_progress_closed() {
        let start = compute_transition_for_issue_type(
            &TransitionAction::Start,
            &TaskStatus::Open,
            None,
            "spike",
        )
        .expect("spike should start from open without an acceptance-criteria gate");
        assert_eq!(start.to_status, Some(TaskStatus::InProgress));

        let close = compute_transition_for_issue_type(
            &TransitionAction::Close,
            &TaskStatus::InProgress,
            None,
            "spike",
        )
        .expect("spike should close from in_progress");
        assert_eq!(close.to_status, Some(TaskStatus::Closed));
    }

    #[test]
    fn stale_rejections_three_cycles_trigger_lead_intervention_at_threshold() {
        let mut status = TaskStatus::InTaskReview;
        let mut continuation_count = 0;

        for _cycle in 1..=3 {
            let stale = compute_transition(&TransitionAction::TaskReviewRejectStale, &status, None)
                .expect("stale reject should be valid from in_task_review");
            assert_eq!(stale.to_status, Some(TaskStatus::Open));
            assert!(stale.increment_continuation);
            continuation_count += 1;
            status = stale.to_status.expect("stale reject should set status");

            if continuation_count >= 3 {
                let escalate = compute_transition(&TransitionAction::Escalate, &status, None)
                    .expect("threshold stale count should allow escalation from open");
                assert_eq!(escalate.to_status, Some(TaskStatus::NeedsLeadIntervention));
                assert!(escalate.reset_continuation);
                status = escalate.to_status.expect("escalate should set status");
                assert_eq!(status, TaskStatus::NeedsLeadIntervention);
            } else {
                let start = compute_transition(&TransitionAction::Start, &status, None)
                    .expect("open should start");
                assert_eq!(start.to_status, Some(TaskStatus::InProgress));
                status = start.to_status.expect("start should set status");

                let submit = compute_transition(&TransitionAction::SubmitTaskReview, &status, None)
                    .expect("in_progress should submit to task review");
                assert_eq!(submit.to_status, Some(TaskStatus::NeedsTaskReview));
                status = submit.to_status.expect("submit should set status");

                let review_start =
                    compute_transition(&TransitionAction::TaskReviewStart, &status, None)
                        .expect("needs_task_review should enter in_task_review");
                assert_eq!(review_start.to_status, Some(TaskStatus::InTaskReview));
                status = review_start
                    .to_status
                    .expect("task_review_start should set status");
            }
        }
    }

    #[test]
    fn met_snapshot_stale_detection_actions_are_distinct() {
        let stale = compute_transition(
            &TransitionAction::TaskReviewRejectStale,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("stale reject should be valid");
        assert!(stale.increment_continuation);
        assert!(!stale.reset_continuation);

        let progress = compute_transition(
            &TransitionAction::TaskReviewReject,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("regular reject should be valid");
        assert!(!progress.increment_continuation);
        assert!(!progress.reset_continuation);

        let conflict = compute_transition(
            &TransitionAction::TaskReviewRejectConflict,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("conflict reject should be valid");
        assert!(!conflict.increment_continuation);
        assert!(conflict.reset_continuation);
    }

    #[test]
    fn conflict_metadata_flags_set_and_cleared() {
        // Conflict transitions set the flag
        let conflict_reject = compute_transition(
            &TransitionAction::TaskReviewRejectConflict,
            &TaskStatus::InTaskReview,
            None,
        )
        .unwrap();
        assert!(conflict_reject.set_merge_conflict_metadata);
        assert!(!conflict_reject.clear_merge_conflict_metadata);

        let pm_conflict = compute_transition(
            &TransitionAction::LeadApproveConflict,
            &TaskStatus::InLeadIntervention,
            None,
        )
        .unwrap();
        assert!(pm_conflict.set_merge_conflict_metadata);
        assert!(!pm_conflict.clear_merge_conflict_metadata);

        // Clearing transitions
        let submit_review = compute_transition(
            &TransitionAction::SubmitTaskReview,
            &TaskStatus::InProgress,
            None,
        )
        .unwrap();
        assert!(submit_review.clear_merge_conflict_metadata);

        let close = compute_transition(&TransitionAction::Close, &TaskStatus::Open, None).unwrap();
        assert!(close.clear_merge_conflict_metadata);

        let force_close =
            compute_transition(&TransitionAction::ForceClose, &TaskStatus::Open, None).unwrap();
        assert!(force_close.clear_merge_conflict_metadata);

        let user_override = compute_transition(
            &TransitionAction::UserOverride,
            &TaskStatus::InProgress,
            Some(&TaskStatus::Open),
        )
        .unwrap();
        assert!(user_override.clear_merge_conflict_metadata);

        // Start does NOT clear
        let start = compute_transition(&TransitionAction::Start, &TaskStatus::Open, None).unwrap();
        assert!(!start.clear_merge_conflict_metadata);
        assert!(!start.set_merge_conflict_metadata);
    }

    // ── CI gate core model tests ───────────────────────────────────────────────

    #[test]
    fn ci_status_serializes_to_exact_wire_strings() {
        assert_eq!(
            serde_json::to_string(&CiStatus::Passing).unwrap(),
            "\"passing\""
        );
        assert_eq!(
            serde_json::to_string(&CiStatus::Failing).unwrap(),
            "\"failing\""
        );
        assert_eq!(
            serde_json::to_string(&CiStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&CiStatus::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn ci_status_deserializes_from_exact_wire_strings() {
        assert_eq!(
            serde_json::from_str::<CiStatus>("\"passing\"").unwrap(),
            CiStatus::Passing
        );
        assert_eq!(
            serde_json::from_str::<CiStatus>("\"failing\"").unwrap(),
            CiStatus::Failing
        );
        assert_eq!(
            serde_json::from_str::<CiStatus>("\"pending\"").unwrap(),
            CiStatus::Pending
        );
        assert_eq!(
            serde_json::from_str::<CiStatus>("\"unknown\"").unwrap(),
            CiStatus::Unknown
        );
    }

    #[test]
    fn ci_status_default_is_unknown() {
        assert_eq!(CiStatus::default(), CiStatus::Unknown);
    }

    #[test]
    fn ci_status_parse_round_trips() {
        for status in [
            CiStatus::Passing,
            CiStatus::Failing,
            CiStatus::Pending,
            CiStatus::Unknown,
        ] {
            assert_eq!(CiStatus::parse(status.as_str()).unwrap(), status);
        }
        assert!(CiStatus::parse("red").is_err());
    }

    #[test]
    fn task_pr_ci_snapshot_defaults_to_unknown() {
        let snapshot = TaskPrCiSnapshot::default();
        assert_eq!(snapshot.ci_status, CiStatus::Unknown);
        assert!(snapshot.blocking_required_check_names.is_empty());
        assert_eq!(snapshot.same_signature_count, 0);
        assert!(snapshot.failure_fingerprint.is_none());
        assert!(snapshot.last_remediation_base_sha.is_none());
    }

    #[test]
    fn task_pr_ci_snapshot_from_input_preserves_fields() {
        let input = TaskPrCiSnapshotInput {
            task_id: "task-1".to_owned(),
            pr_number: 42,
            head_sha: "abc123".to_owned(),
            ci_status: CiStatus::Failing,
            blocking_required_check_names: vec!["Quality Gate".to_owned()],
            failure_fingerprint: Some("abc123:Quality Gate".to_owned()),
            same_signature_count: 3,
            last_remediation_base_sha: Some("base999".to_owned()),
        };
        let snapshot = TaskPrCiSnapshot::from_input(
            input.clone(),
            "2026-01-01T00:00:00Z".to_owned(),
            "2026-01-02T00:00:00Z".to_owned(),
        );

        assert_eq!(snapshot.task_id, "task-1");
        assert_eq!(snapshot.pr_number, 42);
        assert_eq!(snapshot.head_sha, "abc123");
        assert_eq!(snapshot.ci_status, CiStatus::Failing);
        assert_eq!(snapshot.blocking_required_check_names, &["Quality Gate"]);
        assert_eq!(
            snapshot.failure_fingerprint,
            Some("abc123:Quality Gate".to_owned())
        );
        assert_eq!(snapshot.first_seen_at, "2026-01-01T00:00:00Z");
        assert_eq!(snapshot.last_seen_at, "2026-01-02T00:00:00Z");
        assert_eq!(snapshot.same_signature_count, 3);
        assert_eq!(
            snapshot.last_remediation_base_sha,
            Some("base999".to_owned())
        );

        // The input itself should default to unknown when empty.
        let empty_input = TaskPrCiSnapshotInput::default();
        assert_eq!(empty_input.ci_status, CiStatus::Unknown);
    }

    #[test]
    fn task_pr_ci_snapshot_serializes_and_deserializes() {
        let snapshot = TaskPrCiSnapshot {
            task_id: "task-2".to_owned(),
            pr_number: 7,
            head_sha: "deadbeef".to_owned(),
            ci_status: CiStatus::Pending,
            blocking_required_check_names: vec!["Tests".to_owned(), "Lint".to_owned()],
            failure_fingerprint: None,
            first_seen_at: "2026-06-29T12:00:00Z".to_owned(),
            last_seen_at: "2026-06-29T12:00:00Z".to_owned(),
            same_signature_count: 0,
            last_remediation_base_sha: None,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: TaskPrCiSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, snapshot);
        assert!(json.contains("\"ci_status\":\"pending\""));
    }

    // ── Evidence-spike detection tests ──────────────────────────────────────

    #[test]
    fn is_evidence_spike_with_both_labels() {
        // The exact labels stamped by proposal_refinement_demand_evidence (6tjy).
        let labels = r#"["refinement-evidence","read-only","proposal:p1"]"#;
        assert!(is_evidence_spike(labels));
    }

    #[test]
    fn is_evidence_spike_requires_both_markers() {
        // Only refinement-evidence → not enough.
        assert!(!is_evidence_spike(r#"["refinement-evidence"]"#));
        // Only read-only → not enough (this label appears on non-spike tasks too).
        assert!(!is_evidence_spike(r#"["read-only"]"#));
    }

    #[test]
    fn is_evidence_spike_with_extra_labels() {
        // Extra labels don't break detection.
        let labels = r#"["refinement-evidence","read-only","proposal:abc","priority:high"]"#;
        assert!(is_evidence_spike(labels));
    }

    #[test]
    fn is_evidence_spike_empty_labels_is_false() {
        assert!(!is_evidence_spike("[]"));
    }

    #[test]
    fn is_evidence_spike_malformed_json_is_false() {
        // Malformed JSON should fail closed (return false).
        assert!(!is_evidence_spike("not json"));
        assert!(!is_evidence_spike(""));
    }

    #[test]
    fn is_evidence_spike_normal_task_labels_is_false() {
        assert!(!is_evidence_spike(r#"["bug","priority:high"]"#));
        assert!(!is_evidence_spike(r#"["human-review-hold"]"#));
    }

    #[test]
    fn demand_evidence_contract_labels_detected_as_evidence_spike() {
        // The exact labels stamped by `proposal_refinement_demand_evidence`
        // (epic 6tjy) and consumed by the runtime profile selector (xwr4).
        // Both `refinement-evidence` and `read-only` must be present for the
        // evidence-spike profile to be selected.
        let labels = r#"["refinement-evidence","read-only","proposal:p1"]"#;
        assert!(is_evidence_spike(labels));
    }

    #[test]
    fn ordinary_architect_spike_without_read_only_label_is_not_evidence_spike() {
        // A normal Architect spike (issue_type = "spike", agent_type =
        // "architect") that does NOT carry the read-only refinement-evidence
        // contract must NOT be downgraded to the evidence-spike profile.
        assert!(!is_evidence_spike(r#"["spike","priority:high"]"#));
        assert!(!is_evidence_spike(r#"["refinement-evidence"]"#));
        assert!(!is_evidence_spike(r#"["read-only"]"#));
    }

    // ── ReopenClass unit tests ─────────────────────────────────────────────

    #[test]
    fn reopen_class_parse_round_trip() {
        for class in [
            ReopenClass::ReviewRejected,
            ReopenClass::MergeQueueFailed,
            ReopenClass::MergeConflict,
            ReopenClass::Superseded,
            ReopenClass::Infra,
            ReopenClass::Other,
        ] {
            let s = class.as_str();
            assert_eq!(ReopenClass::parse(s), class, "round-trip for {s}");
        }
    }

    #[test]
    fn reopen_class_unknown_parses_to_other() {
        assert_eq!(ReopenClass::parse("bogus"), ReopenClass::Other);
        assert_eq!(ReopenClass::parse(""), ReopenClass::Other);
    }

    #[test]
    fn reopen_class_quality_strike_membership() {
        assert!(ReopenClass::ReviewRejected.is_quality_strike());
        assert!(ReopenClass::MergeQueueFailed.is_quality_strike());
        assert!(ReopenClass::Other.is_quality_strike());
        assert!(!ReopenClass::MergeConflict.is_quality_strike());
        assert!(!ReopenClass::Superseded.is_quality_strike());
        assert!(
            !ReopenClass::Infra.is_quality_strike(),
            "infra must NOT count as a quality strike"
        );
    }

    #[test]
    fn reopen_class_serde_round_trip() {
        for class in [
            ReopenClass::ReviewRejected,
            ReopenClass::MergeQueueFailed,
            ReopenClass::MergeConflict,
            ReopenClass::Superseded,
            ReopenClass::Infra,
            ReopenClass::Other,
        ] {
            let json = serde_json::to_string(&class).unwrap();
            let parsed: ReopenClass = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, class);
        }
    }

    #[test]
    fn reopen_class_infra_round_trips_as_snake_case_string() {
        // AC: Infra round-trips through parse/display/serde.
        assert_eq!(ReopenClass::Infra.as_str(), "infra");
        assert_eq!(ReopenClass::parse("infra"), ReopenClass::Infra);
        assert_eq!(ReopenClass::Infra.to_string(), "infra");
        let json = serde_json::to_string(&ReopenClass::Infra).unwrap();
        assert_eq!(json, "\"infra\"");
        let parsed: ReopenClass = serde_json::from_str("\"infra\"").unwrap();
        assert_eq!(parsed, ReopenClass::Infra);
    }

    #[test]
    fn reopen_class_on_review_reject_transitions() {
        let reject = compute_transition(
            &TransitionAction::TaskReviewReject,
            &TaskStatus::InTaskReview,
            None,
        )
        .unwrap();
        assert_eq!(reject.reopen_class, Some(ReopenClass::ReviewRejected));

        let stale = compute_transition(
            &TransitionAction::TaskReviewRejectStale,
            &TaskStatus::InTaskReview,
            None,
        )
        .unwrap();
        assert_eq!(stale.reopen_class, Some(ReopenClass::ReviewRejected));

        let changes = compute_transition(
            &TransitionAction::PrChangesRequested,
            &TaskStatus::PrReview,
            None,
        )
        .unwrap();
        assert_eq!(changes.reopen_class, Some(ReopenClass::ReviewRejected));
    }

    #[test]
    fn reopen_class_on_conflict_transitions() {
        let conflict = compute_transition(
            &TransitionAction::TaskReviewRejectConflict,
            &TaskStatus::InTaskReview,
            None,
        )
        .unwrap();
        assert_eq!(conflict.reopen_class, Some(ReopenClass::MergeConflict));
        assert!(!conflict.increment_reopen);

        let pr_conflict =
            compute_transition(&TransitionAction::PrConflict, &TaskStatus::Approved, None).unwrap();
        assert_eq!(pr_conflict.reopen_class, Some(ReopenClass::MergeConflict));
        assert!(!pr_conflict.increment_reopen);

        let lead_conflict = compute_transition(
            &TransitionAction::LeadApproveConflict,
            &TaskStatus::InLeadIntervention,
            None,
        )
        .unwrap();
        assert_eq!(lead_conflict.reopen_class, Some(ReopenClass::MergeConflict));
        assert!(!lead_conflict.increment_reopen);
    }

    #[test]
    fn reopen_class_on_pr_ci_failed() {
        let ci_failed =
            compute_transition(&TransitionAction::PrCiFailed, &TaskStatus::PrDraft, None).unwrap();
        assert_eq!(ci_failed.reopen_class, Some(ReopenClass::MergeQueueFailed));
        assert!(ci_failed.increment_reopen);
    }

    #[test]
    fn reopen_class_on_generic_reopen() {
        let reopen =
            compute_transition(&TransitionAction::Reopen, &TaskStatus::Closed, None).unwrap();
        assert_eq!(reopen.reopen_class, Some(ReopenClass::Other));
        assert!(reopen.increment_reopen);
    }

    #[test]
    fn reopen_class_omitted_for_non_reopen_transitions() {
        let start = compute_transition(&TransitionAction::Start, &TaskStatus::Open, None).unwrap();
        assert!(start.reopen_class.is_none());

        let close =
            compute_transition(&TransitionAction::Close, &TaskStatus::InProgress, None).unwrap();
        assert!(close.reopen_class.is_none());

        let release =
            compute_transition(&TransitionAction::Release, &TaskStatus::InProgress, None).unwrap();
        assert!(release.reopen_class.is_none());

        let park = compute_transition(
            &TransitionAction::ParkForRemediation,
            &TaskStatus::PrDraft,
            None,
        )
        .unwrap();
        assert!(park.reopen_class.is_none());
    }

    // ── Arbiter-entry park-rung regressions (7f8u) ────────────────────────────
    //
    // The second-strike park rung in `route_planner_intervention` may observe
    // any non-terminal status.  When the coordinator chooses arbiter dispatch
    // instead of an immediate human-review hold, it will use
    // `TransitionAction::Escalate` to enter `NeedsLeadIntervention` (the Lead
    // arbiter entry point).  These tests lock that contract.

    /// Every non-terminal status the park rung can observe must be accepted by
    /// `Escalate` and must land at `NeedsLeadIntervention`.
    #[test]
    fn escalate_accepts_all_park_rung_source_statuses() {
        // These are the exact statuses `ParkForRemediation` accepts — the same
        // set `route_planner_intervention` may observe at the park rung.
        let park_rung_sources = [
            TaskStatus::Open,
            TaskStatus::InProgress,
            TaskStatus::NeedsTaskReview,
            TaskStatus::InTaskReview,
            TaskStatus::Approved,
            TaskStatus::PrDraft,
            TaskStatus::PrReview,
        ];
        for from in &park_rung_sources {
            let result = compute_transition(&TransitionAction::Escalate, from, None)
                .unwrap_or_else(|e| panic!("Escalate from {from:?} must succeed: {e}"));
            assert_eq!(
                result.to_status,
                Some(TaskStatus::NeedsLeadIntervention),
                "Escalate from {from:?} must land at NeedsLeadIntervention"
            );
            // Escalate resets continuation (matches existing behaviour).
            assert!(
                result.reset_continuation,
                "Escalate from {from:?} must reset continuation"
            );
        }
    }

    /// Terminal and already-in-Lead statuses must reject Escalate with a clear
    /// error, preserving the guard against re-entering an active intervention.
    #[test]
    fn escalate_rejects_terminal_and_lead_statuses() {
        let invalid_sources = [
            TaskStatus::Closed,
            TaskStatus::NeedsLeadIntervention,
            TaskStatus::InLeadIntervention,
        ];
        for from in &invalid_sources {
            let result = compute_transition(&TransitionAction::Escalate, from, None);
            assert!(
                result.is_err(),
                "Escalate from {from:?} must be rejected (terminal or already in Lead)"
            );
            let err_msg = result
                .err()
                .expect("Escalate from invalid source must return Err")
                .to_string();
            assert!(
                err_msg.contains("escalate"),
                "Error message should mention 'escalate': got {err_msg}"
            );
        }
    }

    /// Verify the error message enumerates the full set of valid sources so
    /// callers get a useful diagnostic.
    #[test]
    fn escalate_error_message_lists_all_valid_sources() {
        let err = compute_transition(&TransitionAction::Escalate, &TaskStatus::Closed, None)
            .err()
            .expect("Escalate from Closed must return Err")
            .to_string();
        // Spot-check that every newly-added source appears in the message.
        assert!(
            err.contains("needs_task_review"),
            "message should list needs_task_review"
        );
        assert!(err.contains("approved"), "message should list approved");
        assert!(err.contains("pr_draft"), "message should list pr_draft");
        assert!(err.contains("pr_review"), "message should list pr_review");
    }

    /// Existing Lead intervention transitions continue to behave as before.
    /// (Regression guard: widening Escalate must not break start/release/
    /// complete/approve/conflict.)
    #[test]
    fn lead_intervention_transitions_unchanged() {
        // lead_intervention_start: needs_lead_intervention → in_lead_intervention
        let start = compute_transition(
            &TransitionAction::LeadInterventionStart,
            &TaskStatus::NeedsLeadIntervention,
            None,
        )
        .expect("lead_intervention_start from needs_lead_intervention is valid");
        assert_eq!(start.to_status, Some(TaskStatus::InLeadIntervention));

        // lead_intervention_release: in_lead_intervention → needs_lead_intervention
        let release = compute_transition(
            &TransitionAction::LeadInterventionRelease,
            &TaskStatus::InLeadIntervention,
            None,
        )
        .expect("lead_intervention_release from in_lead_intervention is valid");
        assert_eq!(release.to_status, Some(TaskStatus::NeedsLeadIntervention));

        // lead_intervention_complete: in_lead_intervention → open (+ records intervention)
        let complete = compute_transition(
            &TransitionAction::LeadInterventionComplete,
            &TaskStatus::InLeadIntervention,
            None,
        )
        .expect("lead_intervention_complete from in_lead_intervention is valid");
        assert_eq!(complete.to_status, Some(TaskStatus::Open));
        assert!(complete.reset_continuation);
        assert!(complete.record_intervention);

        // lead_approve: in_lead_intervention → approved
        let approve = compute_transition(
            &TransitionAction::LeadApprove,
            &TaskStatus::InLeadIntervention,
            None,
        )
        .expect("lead_approve from in_lead_intervention is valid");
        assert_eq!(approve.to_status, Some(TaskStatus::Approved));

        // lead_approve_conflict: in_lead_intervention → open (+ merge conflict metadata)
        let conflict = compute_transition(
            &TransitionAction::LeadApproveConflict,
            &TaskStatus::InLeadIntervention,
            None,
        )
        .expect("lead_approve_conflict from in_lead_intervention is valid");
        assert_eq!(conflict.to_status, Some(TaskStatus::Open));
        assert!(conflict.reset_continuation);
        assert!(conflict.set_merge_conflict_metadata);
        assert_eq!(conflict.reopen_class, Some(ReopenClass::MergeConflict));

        // All Lead actions must reject from non-matching statuses.
        for from in &[
            TaskStatus::Open,
            TaskStatus::InProgress,
            TaskStatus::Closed,
            TaskStatus::PrReview,
        ] {
            assert!(
                compute_transition(&TransitionAction::LeadInterventionStart, from, None).is_err(),
                "lead_intervention_start must be invalid from {from:?}"
            );
            assert!(
                compute_transition(&TransitionAction::LeadInterventionRelease, from, None).is_err(),
                "lead_intervention_release must be invalid from {from:?}"
            );
            assert!(
                compute_transition(&TransitionAction::LeadInterventionComplete, from, None)
                    .is_err(),
                "lead_intervention_complete must be invalid from {from:?}"
            );
            assert!(
                compute_transition(&TransitionAction::LeadApprove, from, None).is_err(),
                "lead_approve must be invalid from {from:?}"
            );
            assert!(
                compute_transition(&TransitionAction::LeadApproveConflict, from, None).is_err(),
                "lead_approve_conflict must be invalid from {from:?}"
            );
        }
    }

    /// Grep guard: the only `TransitionAction` variants that produce
    /// `NeedsLeadIntervention` as their target status are `Escalate`
    /// (coordinator arbiter park-rung / second-strike path) and
    /// `LeadInterventionRelease` (coordinator session-recovery release
    /// from `InLeadIntervention` back to queued).  No worker/reviewer
    /// handler or tool path may produce this transition.
    ///
    /// This invariant is the state-machine half of the acceptance
    /// criterion that "production transitions into needs_lead_intervention
    /// are limited to the coordinator arbiter park-rung/state-machine path"
    /// (10qg / aizl).
    #[test]
    fn only_escalate_and_release_produce_needs_lead_intervention() {
        let all_actions = [
            TransitionAction::Start,
            TransitionAction::ResumeWorker,
            TransitionAction::SubmitTaskReview,
            TransitionAction::TaskReviewStart,
            TransitionAction::TaskReviewReject,
            TransitionAction::TaskReviewRejectStale,
            TransitionAction::TaskReviewRejectConflict,
            TransitionAction::TaskReviewApprove,
            TransitionAction::Close,
            TransitionAction::Reopen,
            TransitionAction::Release,
            TransitionAction::ReleaseTaskReview,
            TransitionAction::ForceClose,
            TransitionAction::UserOverride,
            TransitionAction::Escalate,
            TransitionAction::LeadInterventionStart,
            TransitionAction::LeadInterventionRelease,
            TransitionAction::LeadInterventionComplete,
            TransitionAction::LeadApprove,
            TransitionAction::LeadApproveConflict,
            TransitionAction::PrCreated,
            TransitionAction::PrUndraft,
            TransitionAction::PrCiFailed,
            TransitionAction::PrConflict,
            TransitionAction::PrMerge,
            TransitionAction::PrChangesRequested,
            TransitionAction::ParkForRemediation,
            TransitionAction::SubmitForMerge,
            TransitionAction::PreApprovalVerifyRejected,
            TransitionAction::ArbiterPark,
        ];

        // For each action, try every possible source status and record
        // which actions can produce NeedsLeadIntervention.
        let all_statuses = [
            TaskStatus::Open,
            TaskStatus::InProgress,
            TaskStatus::NeedsTaskReview,
            TaskStatus::InTaskReview,
            TaskStatus::Approved,
            TaskStatus::PrDraft,
            TaskStatus::PrReview,
            TaskStatus::NeedsLeadIntervention,
            TaskStatus::InLeadIntervention,
            TaskStatus::Closed,
        ];

        let mut produces_needs_lead = Vec::new();
        for action in &all_actions {
            for from in &all_statuses {
                if let Ok(apply) = compute_transition(action, from, None)
                    && apply.to_status == Some(TaskStatus::NeedsLeadIntervention)
                {
                    produces_needs_lead.push(format!("{action:?} from {from:?}"));
                }
            }
        }

        // Only Escalate and LeadInterventionRelease may produce NeedsLeadIntervention.
        for entry in &produces_needs_lead {
            assert!(
                entry.starts_with("Escalate") || entry.starts_with("LeadInterventionRelease"),
                "unexpected action producing NeedsLeadIntervention: {entry}. \
                 Only Escalate (coordinator arbiter park-rung) and \
                 LeadInterventionRelease (coordinator session-recovery) \
                 may transition to NeedsLeadIntervention"
            );
        }

        // Positive check: Escalate must produce NeedsLeadIntervention from
        // at least one source status (the park-rung sources).
        assert!(
            !produces_needs_lead.is_empty(),
            "at least Escalate must produce NeedsLeadIntervention"
        );

        // Guard the UserOverride backdoor: UserOverride with an explicit
        // target_override of NeedsLeadIntervention (or InLeadIntervention)
        // must be rejected for ALL source statuses.  Without this check,
        // compute_transition(action, from, None) silently skips the
        // UserOverride path (which requires Some(target_override)) and
        // the test would miss the backdoor.
        for from in &all_statuses {
            assert!(
                compute_transition(
                    &TransitionAction::UserOverride,
                    from,
                    Some(&TaskStatus::NeedsLeadIntervention),
                )
                .is_err(),
                "UserOverride must NOT target NeedsLeadIntervention from {from:?}; \
                 the coordinator arbiter park-rung (Escalate) is the only entry"
            );
            assert!(
                compute_transition(
                    &TransitionAction::UserOverride,
                    from,
                    Some(&TaskStatus::InLeadIntervention),
                )
                .is_err(),
                "UserOverride must NOT target InLeadIntervention from {from:?}; \
                 LeadInterventionStart from NeedsLeadIntervention is the only entry"
            );
        }
    }
}
